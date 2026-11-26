use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use orca_core::tool_types::{
    FileChangePreview, FileEffect, ToolRequest, ToolResult, truncate_output,
};
use orca_platform::fs::{
    FileAccess, FileIdentity, VerifiedDirectory, file_links, file_mode, stale_path,
};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::file_admission::{
    MAX_DIFF_OUTPUT_BYTES, MAX_EDIT_FILE_BYTES, build_file_change_preview_with_headers,
    read_utf8_reader_with_limit,
};
use crate::resolve_workspace_path;

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_OPERATIONS: usize = 100;
const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;
const MAX_MATCH_WORK: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct PatchError(String);

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for PatchError {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

pub(crate) struct Patch {
    pub(crate) ops: Vec<PatchOp>,
}

pub(crate) enum PatchOp {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

pub(crate) struct Hunk {
    pub(crate) context_hint: Option<String>,
    pub(crate) lines: Vec<HunkLine>,
    end_of_file: bool,
}

pub(crate) enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

pub(crate) fn parse_patch(input: &str) -> Result<Patch, PatchError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err("patch input exceeds 1 MiB".into());
    }
    let lines: Vec<_> = input.lines().collect();
    let start = lines.iter().position(|line| !line.trim().is_empty());
    let end = lines.iter().rposition(|line| !line.trim().is_empty());
    let (Some(start), Some(end)) = (start, end) else {
        return Err("patch is empty".into());
    };
    if lines[start] != "*** Begin Patch" || lines[end] != "*** End Patch" {
        return Err("expected *** Begin Patch and *** End Patch".into());
    }

    let mut body = lines[start + 1..end].iter().copied().peekable();
    let mut ops = Vec::new();
    while let Some(line) = body.next() {
        if ops.len() == MAX_OPERATIONS {
            return Err("patch exceeds 100 operations".into());
        }
        let op = (|| {
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                let path = parse_path(path)?;
                let mut lines = Vec::new();
                while let Some(text) = body.peek().and_then(|line| line.strip_prefix('+')) {
                    lines.push(text.to_owned());
                    body.next();
                }
                Ok(PatchOp::Add { path, lines })
            } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
                Ok(PatchOp::Delete {
                    path: parse_path(path)?,
                })
            } else if let Some(path) = line.strip_prefix("*** Update File: ") {
                let path = parse_path(path)?;
                let move_to = if let Some(dest) = body
                    .peek()
                    .and_then(|line| line.strip_prefix("*** Move to: "))
                {
                    body.next();
                    Some(parse_path(dest)?)
                } else {
                    None
                };
                let mut hunks = Vec::new();
                while let Some(header) =
                    body.next_if(|line| *line == "@@" || line.starts_with("@@ "))
                {
                    hunks.push(parse_hunk(header, &mut body)?);
                }
                if hunks.is_empty() && move_to.is_none() {
                    return Err("expected at least one @@ hunk");
                }
                Ok(PatchOp::Update {
                    path,
                    move_to,
                    hunks,
                })
            } else {
                Err("expected an Add File, Delete File or Update File header")
            }
        })()
        .map_err(|reason| PatchError(format!("operation {} ({line}): {reason}", ops.len() + 1)))?;
        ops.push(op);
    }
    if ops.is_empty() {
        return Err("patch has no file operations".into());
    }
    Ok(Patch { ops })
}

fn parse_path(path: &str) -> Result<String, &'static str> {
    let path = path.trim();
    if path.is_empty() || path.contains('\0') {
        return Err("invalid file path");
    }
    Ok(path.to_owned())
}

fn parse_hunk<'a>(
    header: &str,
    body: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Result<Hunk, &'static str> {
    let context_hint = header
        .strip_prefix("@@ ")
        .filter(|hint| !hint.is_empty())
        .map(str::to_owned);
    let mut lines = Vec::new();
    while let Some(line) = body.next_if(|line| line.starts_with([' ', '+', '-'])) {
        let text = line[1..].to_owned();
        lines.push(match line.as_bytes()[0] {
            b' ' => HunkLine::Context(text),
            b'-' => HunkLine::Remove(text),
            _ => HunkLine::Add(text),
        });
    }
    if lines.is_empty() {
        return Err("hunk has no lines");
    }
    if lines
        .iter()
        .all(|line| matches!(line, HunkLine::Context(_)))
    {
        return Err("hunk has no additions or removals");
    }
    let end_of_file = body.next_if_eq(&"*** End of File").is_some();
    Ok(Hunk {
        context_hint,
        lines,
        end_of_file,
    })
}

#[derive(Debug)]
pub struct PreparedPatch {
    root: PathBuf,
    root_identity: FileIdentity,
    ops: Vec<PlannedOp>,
}

#[derive(Debug)]
pub(crate) struct PlannedTarget {
    pub(crate) path: PathBuf,
    pub(crate) label: String,
    ancestor: PathBuf,
    ancestor_identity: FileIdentity,
    case_insensitive: bool,
    relative: String,
    requested_relative: String,
    mode: Option<u32>,
    identity: Option<FileIdentity>,
}

#[derive(Debug)]
pub(crate) struct PlannedOp {
    pub(crate) target: PlannedTarget,
    pub(crate) move_to: Option<PlannedTarget>,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
    added: usize,
    removed: usize,
}

#[cfg(test)]
fn preflight(patch: &Patch, cwd: &Path) -> Result<PreparedPatch, PatchError> {
    preflight_or_cancel(patch, cwd, &|| false)
}

fn preflight_or_cancel(
    patch: &Patch,
    cwd: &Path,
    should_cancel: &dyn Fn() -> bool,
) -> Result<PreparedPatch, PatchError> {
    let root = cwd
        .canonicalize()
        .map_err(|error| PatchError(format!("cannot resolve workspace: {error}")))?;
    let root_identity = VerifiedDirectory::open(&root)
        .and_then(|directory| directory.identity())
        .map_err(|error| PatchError(format!("cannot verify workspace: {error}")))?;
    let mut identities = HashSet::new();
    let mut retained = 0;
    let mut matching = MatchBudget {
        remaining: MAX_MATCH_WORK,
        should_cancel,
    };
    let mut seen = HashSet::new();
    let mut ops = Vec::with_capacity(patch.ops.len());
    for (index, op) in patch.ops.iter().enumerate() {
        let (name, path, destination) = match op {
            PatchOp::Add { path, .. } => ("add", path, None),
            PatchOp::Delete { path } => ("delete", path, None),
            PatchOp::Update { path, move_to, .. } => ("update", path, move_to.as_deref()),
        };
        let planned = (|| {
            if should_cancel() {
                return Err("patch cancelled".into());
            }
            let removes_source = matches!(op, PatchOp::Delete { .. }) || destination.is_some();
            let mut target = resolve_target(cwd, &root, path, removes_source, &mut seen)?;
            let move_to = destination
                .map(|dest| resolve_target(cwd, &root, dest, false, &mut seen))
                .transpose()?;
            if let Some(dest) = &move_to {
                require_missing(dest)?;
            }
            let (before, after, (added, removed)) = match op {
                PatchOp::Add { lines, .. } => {
                    require_missing(&target)?;
                    let mut text = lines.join("\n");
                    if !lines.is_empty() {
                        text.push('\n');
                    }
                    (None, Some(text), (lines.len(), 0))
                }
                PatchOp::Delete { .. } => {
                    let before = read_existing(&mut target, should_cancel)?;
                    let removed = before.lines().count();
                    (Some(before), None, (0, removed))
                }
                PatchOp::Update { hunks, .. } => {
                    let before = read_existing(&mut target, should_cancel)?;
                    let after = apply_hunks(&before, hunks, &mut matching)?;
                    let counts = hunks.iter().flat_map(|hunk| &hunk.lines).fold(
                        (0, 0),
                        |(added, removed), line| match line {
                            HunkLine::Add(_) => (added + 1, removed),
                            HunkLine::Remove(_) => (added, removed + 1),
                            HunkLine::Context(_) => (added, removed),
                        },
                    );
                    (Some(before), Some(after), counts)
                }
            };
            if let Some(after) = &after
                && after.len() > MAX_EDIT_FILE_BYTES
            {
                return Err(format!("result exceeds {MAX_EDIT_FILE_BYTES} bytes"));
            }
            if let Some(identity) = target.identity
                && !identities.insert(identity)
            {
                return Err(format!("repeated path: {path}"));
            }
            retained +=
                before.as_ref().map_or(0, String::len) + after.as_ref().map_or(0, String::len);
            if retained > MAX_RETAINED_BYTES {
                return Err("patch before-and-after text exceeds 64 MiB".into());
            }
            Ok(PlannedOp {
                target,
                move_to,
                before,
                after,
                added,
                removed,
            })
        })()
        .map_err(|reason: String| {
            PatchError(format!("operation {} ({name} {path}): {reason}", index + 1))
        })?;
        ops.push(planned);
    }
    Ok(PreparedPatch {
        root,
        root_identity,
        ops,
    })
}

fn resolve_target(
    cwd: &Path,
    root: &Path,
    label: &str,
    removes_source: bool,
    seen: &mut HashSet<PathBuf>,
) -> Result<PlannedTarget, String> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let requested = Path::new(label);
        if matches!(requested.components().next(), Some(Component::Prefix(prefix))
            if !matches!(prefix.kind(), Prefix::Disk(_)) || !requested.is_absolute())
            || (requested.has_root() && !requested.is_absolute())
        {
            return Err(format!("unsupported Windows path: {label}"));
        }
        let full = cwd.join(requested);
        orca_platform::fs::PathPolicy::windows_sandbox()
            .identity(&full.to_string_lossy())
            .map_err(|error| error.to_string())?;
    }
    let path = resolve_workspace_path(cwd, Some(label))?;
    let mut ancestor = path.as_path();
    let (canonical, directory) = loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                let canonical = ancestor
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve {label}: {error}"))?;
                if !canonical.starts_with(root) {
                    return Err(format!("path escapes workspace: {label}"));
                }
                if ancestor == path {
                    if removes_source && metadata.is_symlink() {
                        return Err(format!("cannot delete or move symbolic link {label}"));
                    }
                    let directory = canonical.parent().unwrap_or(root).to_path_buf();
                    break (canonical, directory);
                }
                if !metadata.is_dir() && !canonical.is_dir() {
                    return Err(format!("a parent of {label} is not a directory"));
                }
                let resolved = canonical.join(
                    path.strip_prefix(ancestor)
                        .expect("ancestor of resolved path"),
                );
                break (resolved, canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| format!("cannot resolve {label}: {error}"))?;
            }
            Err(error) => return Err(format!("cannot resolve {label}: {error}")),
        }
    };
    let verified = VerifiedDirectory::open(&directory).map_err(|error| error.to_string())?;
    let case_insensitive = verified.ignores_case(&directory);
    let key = path_key(&canonical, case_insensitive);
    if seen.contains(&key) {
        return Err(format!("repeated path: {label}"));
    }
    if seen
        .iter()
        .any(|previous| previous.starts_with(&key) || key.starts_with(previous))
    {
        return Err(format!(
            "file path conflicts with another operation: {label}"
        ));
    }
    let relative = workspace_label(root, &canonical)?;
    let requested_relative = workspace_label(cwd, &path)?;
    if relative.is_empty() {
        return Err("patch target must name a file inside the workspace".into());
    }
    seen.insert(key);
    Ok(PlannedTarget {
        path: canonical,
        label: label.to_owned(),
        ancestor: directory,
        ancestor_identity: verified.identity().map_err(|error| error.to_string())?,
        case_insensitive,
        relative,
        requested_relative,
        mode: None,
        identity: None,
    })
}

fn workspace_label(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .map(|label| {
            if cfg!(windows) {
                label.replace('\\', "/")
            } else {
                label.to_owned()
            }
        })
        .ok_or_else(|| {
            format!(
                "cannot express {} relative to the workspace",
                path.display()
            )
        })
}

fn path_key(path: &Path, case_insensitive: bool) -> PathBuf {
    let name = path.to_string_lossy();
    let normalized: String = name.nfc().collect();
    if case_insensitive {
        PathBuf::from(normalized.as_str().case_fold().nfc().collect::<String>())
    } else {
        PathBuf::from(normalized)
    }
}

fn require_missing(target: &PlannedTarget) -> Result<(), String> {
    match target.path.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => Err(format!("{} already exists", target.label)),
        Err(error) => Err(format!("cannot check {}: {error}", target.label)),
    }
}

fn read_existing(
    target: &mut PlannedTarget,
    should_cancel: &dyn Fn() -> bool,
) -> Result<String, String> {
    let read = || -> io::Result<(FileIdentity, Option<u32>, String)> {
        let directory = VerifiedDirectory::open(&target.ancestor)?;
        if directory.identity()? != target.ancestor_identity
            || target.path.parent() != Some(&target.ancestor)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "file or parent is missing",
            ));
        }
        let file = directory.open_file(target.file_name(), FileAccess::Read)?;
        let identity = single_link_identity(&file)?;
        Ok((
            identity,
            file_mode(&file)?,
            read_opened(&file, should_cancel)?,
        ))
    };
    let (identity, mode, before) = read().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            format!("{} does not exist", target.label)
        } else {
            format!("cannot read {}: {error}", target.label)
        }
    })?;
    target.identity = Some(identity);
    target.mode = mode;
    Ok(before)
}

fn single_link_identity(file: &fs::File) -> io::Result<FileIdentity> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    let links = file_links(file)?;
    if links.count > 1 {
        return Err(io::Error::other("file has more than one hard link"));
    }
    Ok(links.identity)
}

fn read_opened(file: &fs::File, should_cancel: &dyn Fn() -> bool) -> io::Result<String> {
    let declared = file.metadata()?.len();
    let max = MAX_EDIT_FILE_BYTES as u64;
    if declared > max {
        return Err(io::Error::other(format!("file exceeds {max} bytes")));
    }
    read_utf8_reader_with_limit(file, declared, max, should_cancel).map_err(io::Error::other)
}

impl PlannedTarget {
    fn file_name(&self) -> &std::ffi::OsStr {
        self.path.file_name().expect("patch target has a file name")
    }

    fn parent_for_write(
        &self,
        plan: &PreparedPatch,
        created: &mut HashMap<PathBuf, FileIdentity>,
        effects: &mut Vec<FileEffect>,
    ) -> io::Result<VerifiedDirectory> {
        // Unix directory descriptors keep operations on the checked directory
        // even if another process moves it after the walk.
        let mut directory = VerifiedDirectory::open(&plan.root)?;
        if directory.identity()? != plan.root_identity {
            return Err(stale_path());
        }
        let mut path = plan.root.clone();
        for component in self
            .ancestor
            .strip_prefix(&plan.root)
            .expect("checked workspace ancestor")
            .iter()
        {
            directory = directory.child(component)?;
            path.push(component);
        }
        if directory.identity()? != self.ancestor_identity {
            return Err(stale_path());
        }
        let parent = self.path.parent().expect("patch target has a parent");
        for component in parent
            .strip_prefix(&path)
            .expect("checked target ancestor")
            .iter()
        {
            let next = path.join(component);
            let key = path_key(&next, self.case_insensitive);
            if let Some(identity) = created.get(&key) {
                directory = directory.child(component)?;
                if directory.identity()? != *identity {
                    return Err(stale_path());
                }
            } else {
                let label = workspace_label(&plan.root, &next).map_err(io::Error::other)?;
                directory.create_directory(component)?;
                effects.push(FileEffect::CreatedDirectory(label));
                directory = directory.child(component)?;
                created.insert(key, directory.identity()?);
            }
            path = next;
        }
        Ok(directory)
    }

    fn verify_existing(
        &self,
        directory: &VerifiedDirectory,
        access: FileAccess,
        before: &str,
    ) -> io::Result<fs::File> {
        let file = directory.open_file(self.file_name(), access)?;
        if Some(single_link_identity(&file)?) != self.identity
            || read_opened(&file, &|| false)? != before
        {
            return Err(stale_path());
        }
        single_link_identity(&file)?;
        Ok(file)
    }
}

fn apply_hunks(
    original: &str,
    hunks: &[Hunk],
    budget: &mut MatchBudget<'_>,
) -> Result<String, String> {
    if hunks.is_empty() {
        return Ok(original.to_owned());
    }
    let source: Vec<_> = original.lines().collect();
    let mut result = Vec::new();
    let mut cursor = 0;
    let mut trailing_blank_insertion = false;
    for (index, hunk) in hunks.iter().enumerate() {
        let start = if let Some(hint) = &hunk.context_hint {
            find_match(&source, cursor, &[hint.as_str()], false, budget)
                .map_err(|reason| format!("hunk {} hint: {reason}", index + 1))?
                + 1
        } else {
            cursor
        };
        let old: Vec<_> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let offset = if old.is_empty() {
            if hunk.end_of_file || hunk.context_hint.is_none() {
                source.len()
            } else {
                start
            }
        } else {
            find_match(&source, start, &old, hunk.end_of_file, budget)
                .map_err(|reason| format!("hunk {}: {reason}", index + 1))?
        };
        if cursor < offset {
            trailing_blank_insertion = false;
            result.extend_from_slice(&source[cursor..offset]);
        }
        cursor = offset;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    trailing_blank_insertion = false;
                    result.push(source[cursor]);
                    cursor += 1;
                }
                HunkLine::Remove(_) => cursor += 1,
                HunkLine::Add(text) => {
                    trailing_blank_insertion = text.is_empty();
                    result.push(text);
                }
            }
        }
    }
    result.extend_from_slice(&source[cursor..]);
    let newline_count = original.bytes().filter(|byte| *byte == b'\n').count();
    let ending = if original.match_indices("\r\n").count() > newline_count / 2 {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = result.join(ending);
    if !result.is_empty()
        && (original.is_empty()
            || original.ends_with('\n')
            || (cursor == source.len() && trailing_blank_insertion))
    {
        updated.push_str(ending);
    }
    Ok(updated)
}

struct MatchBudget<'a> {
    remaining: usize,
    should_cancel: &'a dyn Fn() -> bool,
}

impl MatchBudget<'_> {
    fn compare(&mut self, actual: &str, expected: &str) -> Result<(), &'static str> {
        if (self.should_cancel)() {
            return Err("patch cancelled");
        }
        // Trimming can scan both strings, so charge input bytes as well as comparisons.
        let work = 16 + actual.len() + expected.len();
        self.remaining = self
            .remaining
            .checked_sub(work)
            .ok_or("patch matching work limit exceeded")?;
        Ok(())
    }
}

fn find_match(
    source: &[&str],
    start: usize,
    old: &[&str],
    end_of_file: bool,
    budget: &mut MatchBudget<'_>,
) -> Result<usize, &'static str> {
    let last = source
        .len()
        .checked_sub(old.len())
        .filter(|last| *last >= start)
        .ok_or("context was not found")?;
    let first = if end_of_file { last } else { start };
    let normalizers: [fn(&str) -> &str; 3] = [|line| line, str::trim_end, str::trim];
    for (level, normalize) in normalizers.into_iter().enumerate() {
        let mut found = None;
        for offset in first..=last {
            if lines_match(&source[offset..], old, normalize, budget)?
                && found.replace(offset).is_some()
            {
                return Err("context matched multiple locations");
            }
        }
        if let Some(offset) = found {
            if level > 0 {
                for earlier in 0..start {
                    if lines_match(&source[earlier..], old, normalizers[0], budget)? {
                        return Err("ambiguous context: exact match before the current position");
                    }
                }
            }
            return Ok(offset);
        }
    }
    Err("context was not found")
}

fn lines_match(
    source: &[&str],
    old: &[&str],
    normalize: fn(&str) -> &str,
    budget: &mut MatchBudget<'_>,
) -> Result<bool, &'static str> {
    for (actual, expected) in source.iter().zip(old) {
        budget.compare(actual, expected)?;
        if normalize(actual) != normalize(expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(serde::Deserialize)]
struct PatchArgs {
    input: String,
}

fn parse_request(request: &ToolRequest) -> Result<Patch, PatchError> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or("missing patch arguments")?;
    // A JSON escape can occupy six bytes per decoded byte.
    if raw.len() > 6 * MAX_INPUT_BYTES + 1024 {
        return Err("patch input exceeds 1 MiB".into());
    }
    let args: PatchArgs = serde_json::from_str(raw)
        .map_err(|error| PatchError(format!("invalid patch arguments: {error}")))?;
    parse_patch(&args.input)
}

/// Prepare the paths and file contents once for this invocation.
pub fn prepare(
    request: &ToolRequest,
    cwd: &Path,
    should_cancel: impl Fn() -> bool,
) -> Result<PreparedPatch, PatchError> {
    preflight_or_cancel(&parse_request(request)?, cwd, &should_cancel)
}

impl PreparedPatch {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn approval_targets(&self) -> impl Iterator<Item = &str> {
        self.ops
            .iter()
            .flat_map(|op| std::iter::once(&op.target).chain(op.move_to.as_ref()))
            .flat_map(|target| {
                std::iter::once(target.relative.as_str()).chain(
                    (target.requested_relative != target.relative)
                        .then_some(target.requested_relative.as_str()),
                )
            })
    }

    pub fn pre_images(&self) -> impl Iterator<Item = (&str, Option<(&str, Option<u32>)>)> {
        self.ops.iter().flat_map(|op| {
            let source = (
                &*op.target.relative,
                op.before.as_deref().map(|before| (before, op.target.mode)),
            );
            std::iter::once(source).chain(op.move_to.as_ref().map(|dest| (&*dest.relative, None)))
        })
    }

    pub fn preview(&self) -> String {
        patch_diff(&self.ops)
    }

    pub fn commit(&self, request: &ToolRequest, should_cancel: impl Fn() -> bool) -> ToolResult {
        commit(request, self, &should_cancel)
    }
}

pub(crate) fn execute(
    request: &ToolRequest,
    cwd: &Path,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    match prepare(request, cwd, &should_cancel) {
        Ok(plan) => plan.commit(request, &should_cancel),
        Err(error) => ToolResult::failed(request, format!("patch preflight failed: {error}"), None),
    }
}

fn commit(
    request: &ToolRequest,
    plan: &PreparedPatch,
    should_cancel: &dyn Fn() -> bool,
) -> ToolResult {
    let mut written = Vec::new();
    let mut effects = Vec::new();
    let mut created = HashMap::new();
    for (index, op) in plan.ops.iter().enumerate() {
        let first_effect = effects.len();
        let cancelled = should_cancel();
        let outcome = if cancelled {
            Err(io::Error::new(io::ErrorKind::Interrupted, "patch cancelled").into())
        } else {
            op.write(plan, &mut created, &mut effects)
        };
        for effect in &effects[first_effect..] {
            if let FileEffect::CreatedDirectory(path) = effect {
                written.push(format!("created directory {path}"));
            }
        }
        if let Err(WriteFailure { error, changed }) = outcome {
            let failed = if cancelled {
                "(none)".to_owned()
            } else {
                op.label()
            };
            let pending = plan.ops[index + usize::from(!cancelled)..]
                .iter()
                .map(PlannedOp::label)
                .collect::<Vec<_>>();
            let mut result = if cancelled && effects.is_empty() {
                ToolResult::cancelled_before_start(request, "patch cancelled")
            } else {
                let message = if cancelled {
                    error.to_string()
                } else {
                    format!("failed to apply {failed}: {error}")
                };
                ToolResult::failed(request, message, None)
            };
            let written = if written.is_empty() {
                "(none)".into()
            } else {
                written.join("\n")
            };
            let pending = if pending.is_empty() {
                "(none)".into()
            } else {
                pending.join(", ")
            };
            result.output = Some(format!(
                "Written:\n{written}\nFailed: {failed}\nNot attempted: {pending}\nReason: {error}"
            ));
            result.file_effects = effects;
            let mut diff = patch_diff(&plan.ops[..index]);
            if let Some(target) = changed {
                if let Some(output) = &mut result.output {
                    output.push_str(&format!("\nPartially written: {}", target.label));
                }
                diff.push_str(&op.partial_preview(target));
                diff = truncate_output(diff, MAX_DIFF_OUTPUT_BYTES).0;
            }
            return if diff.is_empty() {
                result
            } else {
                result.with_file_change_preview(FileChangePreview::Patch { text: diff })
            };
        }
        written.push(op.summary());
    }
    let mut result = ToolResult::completed(request, written.join("\n"), false);
    result.file_effects = effects;
    with_committed_preview(result, &plan.ops)
}

fn with_committed_preview(result: ToolResult, committed: &[PlannedOp]) -> ToolResult {
    if committed.is_empty() {
        return result;
    }
    result.with_file_change_preview(FileChangePreview::Patch {
        text: patch_diff(committed),
    })
}

fn patch_diff(ops: &[PlannedOp]) -> String {
    let mut diff = String::new();
    for op in ops {
        diff.push_str(&op.preview());
        if diff.len() > MAX_DIFF_OUTPUT_BYTES {
            return truncate_output(diff, MAX_DIFF_OUTPUT_BYTES).0;
        }
    }
    diff
}

impl PlannedOp {
    fn label(&self) -> String {
        match &self.move_to {
            Some(dest) => format!("{} -> {}", self.target.label, dest.label),
            None => self.target.label.clone(),
        }
    }

    fn summary(&self) -> String {
        let kind = match (&self.move_to, &self.before, &self.after) {
            (Some(_), _, _) => "moved",
            (_, None, _) => "added",
            (_, _, None) => "deleted",
            _ => "updated",
        };
        format!(
            "{kind} {} (+{} -{})",
            self.label(),
            self.added,
            self.removed
        )
    }

    fn write(
        &self,
        plan: &PreparedPatch,
        created: &mut HashMap<PathBuf, FileIdentity>,
        effects: &mut Vec<FileEffect>,
    ) -> Result<(), WriteFailure<'_>> {
        let parent = self.target.parent_for_write(plan, created, effects)?;
        match (&self.move_to, &self.before, &self.after) {
            (_, Some(before), None) => {
                let file = self
                    .target
                    .verify_existing(&parent, FileAccess::Delete, before)?;
                parent.remove_file(self.target.file_name(), &file)?;
                effects.push(FileEffect::Deleted(self.target.relative.clone()));
                Ok(())
            }
            (Some(dest), Some(before), Some(text)) => {
                let file = self
                    .target
                    .verify_existing(&parent, FileAccess::Delete, before)?;
                let permissions = file.metadata()?.permissions();
                let destination = dest.parent_for_write(plan, created, effects)?;
                create_file(&destination, dest, text, effects)?
                    .set_permissions(permissions)
                    .map_err(|error| WriteFailure::after_write(error, dest))?;
                parent
                    .remove_file(self.target.file_name(), &file)
                    .map_err(|error| WriteFailure::after_write(error, dest))?;
                effects.push(FileEffect::Deleted(self.target.relative.clone()));
                Ok(())
            }
            (_, None, Some(text)) => create_file(&parent, &self.target, text, effects).map(|_| ()),
            (_, Some(before), Some(text)) => {
                let mut file = self
                    .target
                    .verify_existing(&parent, FileAccess::Update, before)?;
                // A writer holding the same inode can still edit it or add a
                // hard link between validation and truncation.
                file.set_len(0)?;
                effects.push(FileEffect::Written(self.target.relative.clone()));
                use std::io::{Seek, SeekFrom};
                file.seek(SeekFrom::Start(0))
                    .and_then(|_| file.write_all(text.as_bytes()))
                    .map_err(|error| WriteFailure::after_write(error, &self.target))
            }
            _ => unreachable!("validated patch operation"),
        }
    }

    fn partial_preview(&self, target: &PlannedTarget) -> String {
        let after = target
            .path
            .parent()
            .ok_or_else(stale_path)
            .and_then(VerifiedDirectory::open)
            .and_then(|directory| directory.open_file(target.file_name(), FileAccess::Read))
            .and_then(|file| read_opened(&file, &|| false));
        let Ok(after) = after else {
            return format!("[Diff preview unavailable for {}]\n", target.label);
        };
        let before = if target.path == self.target.path {
            self.before.as_deref()
        } else {
            None
        };
        let old_header = if before.is_some() {
            format!("a/{}", target.label)
        } else {
            "/dev/null".into()
        };
        let new_header = format!("b/{}", target.label);
        render_preview(
            &target.label,
            &old_header,
            &new_header,
            before,
            Some(&after),
        )
    }

    fn preview(&self) -> String {
        let old_header = if self.before.is_some() {
            format!("a/{}", self.target.label)
        } else {
            "/dev/null".into()
        };
        let new_header = if self.after.is_some() {
            format!("b/{}", self.move_to.as_ref().unwrap_or(&self.target).label)
        } else {
            "/dev/null".into()
        };
        render_preview(
            &self.target.label,
            &old_header,
            &new_header,
            self.before.as_deref(),
            self.after.as_deref(),
        )
    }
}

fn render_preview(
    label: &str,
    old_header: &str,
    new_header: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> String {
    let preview =
        build_file_change_preview_with_headers(label, old_header, new_header, before, after);
    if matches!(&preview, FileChangePreview::UnifiedDiff { text, .. } if text.is_empty()) {
        return format!("--- {old_header}\n+++ {new_header}\n");
    }
    let mut text = preview.display_text();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

struct WriteFailure<'a> {
    error: io::Error,
    changed: Option<&'a PlannedTarget>,
}

impl From<io::Error> for WriteFailure<'_> {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            changed: None,
        }
    }
}

impl<'a> WriteFailure<'a> {
    fn after_write(error: io::Error, target: &'a PlannedTarget) -> Self {
        Self {
            error,
            changed: Some(target),
        }
    }
}

fn create_file<'a>(
    directory: &VerifiedDirectory,
    target: &'a PlannedTarget,
    text: &str,
    effects: &mut Vec<FileEffect>,
) -> Result<fs::File, WriteFailure<'a>> {
    let mut file = directory.create_file(target.file_name())?;
    effects.push(FileEffect::Written(target.relative.clone()));
    file.write_all(text.as_bytes())
        .map_err(|error| WriteFailure::after_write(error, target))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn request(body: &str) -> orca_core::tool_types::ToolRequest {
        use orca_core::{
            approval_types::ActionKind,
            tool_types::{ToolName, ToolRequest},
        };
        ToolRequest {
            id: "patch-test".into(),
            name: ToolName::plain("apply_patch"),
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(
                serde_json::json!({"input": format!("*** Begin Patch\n{body}\n*** End Patch")})
                    .to_string(),
            ),
        }
    }

    #[test]
    fn apply_patch_applies_a_multi_file_patch() {
        use orca_core::tool_types::ToolStatus;
        let dir = TempDir::new().unwrap();
        for (name, text) in [
            ("update", "old\n"),
            ("delete", "remove\n"),
            ("move", "before\n"),
        ] {
            fs::write(dir.path().join(name), text).unwrap();
        }
        let execute = |body| {
            crate::registry::tool_registry_for_session(None, &[], true)
                .execute(&request(body), &crate::ToolContext::new(dir.path()))
        };
        let rejected =
            execute("*** Add File: untouched\n+new\n*** Update File: update\n@@\n-missing\n+new");
        assert_eq!(rejected.status, ToolStatus::Failed);
        assert!(!dir.path().join("untouched").exists());

        let result = execute(
            "*** Add File: nested/new\n+hello\n*** Update File: update\n@@\n-old\n+new\n*** Delete File: delete\n*** Update File: move\n*** Move to: moved\n@@\n-before\n+after",
        );
        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        for (name, text) in [
            ("nested/new", "hello\n"),
            ("update", "new\n"),
            ("moved", "after\n"),
        ] {
            assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), text);
        }
        assert!(!dir.path().join("delete").exists());
        assert!(!dir.path().join("move").exists());
        let output = result.output.unwrap();
        for change in [
            "added nested/new (+1 -0)",
            "updated update (+1 -1)",
            "deleted delete (+0 -1)",
            "moved move -> moved (+1 -1)",
        ] {
            assert!(output.contains(change), "{output}");
        }
        let preview = result.file_change_preview.unwrap();
        let diff = preview.patch_diff().expect("multi-file preview");
        for header in [
            "+++ b/nested/new",
            "+++ b/update",
            "--- a/delete",
            "--- a/move",
            "+++ b/moved",
        ] {
            assert!(diff.contains(header), "{diff}");
        }
    }

    #[test]
    fn a_commit_failure_reports_written_failed_and_not_attempted() {
        use orca_core::tool_types::ToolStatus;
        let dir = TempDir::new().unwrap();
        let request = request(
            "*** Add File: one\n+1\n*** Add File: two\n+2\n*** Add File: three\n+3\n*** Add File: four\n+4\n*** Add File: five\n+5",
        );
        let patch = parse_request(&request).unwrap();
        let planned = preflight(&patch, dir.path()).unwrap();
        // A file appears between the preflight and the first write.
        fs::write(dir.path().join("three"), "existing\n").unwrap();

        let result = commit(&request, &planned, &|| false);
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.output.as_deref().unwrap().starts_with(
            "Written:\nadded one (+1 -0)\nadded two (+1 -0)\nFailed: three\nNot attempted: four, five\nReason: "
        ));
        assert!(result.error.as_deref().unwrap().contains("three"));
        assert_eq!(fs::read_to_string(dir.path().join("one")).unwrap(), "1\n");
        assert_eq!(fs::read_to_string(dir.path().join("two")).unwrap(), "2\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("three")).unwrap(),
            "existing\n"
        );
        assert!(!dir.path().join("four").exists());
        assert!(!dir.path().join("five").exists());
    }

    fn plan(body: &str, cwd: &Path) -> Result<PreparedPatch, PatchError> {
        preflight(
            &parse_patch(&format!("*** Begin Patch\n{body}\n*** End Patch")).unwrap(),
            cwd,
        )
    }

    #[test]
    fn parses_add_delete_update_and_move() {
        let patch = parse_patch("\n*** Begin Patch\n*** Add File: a\n+new\n*** Delete File: b\n*** Update File: c\n*** Move to: d\n@@ anchor\n context\n-old\n+new\n*** End of File\n*** End Patch\n\n").unwrap();
        assert!(
            matches!(&patch.ops[..], [PatchOp::Add { path: a, lines }, PatchOp::Delete { path: b }, PatchOp::Update { path: c, move_to: Some(d), hunks }]
            if a == "a" && lines == &["new"] && b == "b" && c == "c" && d == "d"
                && hunks[0].context_hint.as_deref() == Some("anchor") && hunks[0].end_of_file
                && matches!(&hunks[0].lines[..], [HunkLine::Context(context), HunkLine::Remove(old), HunkLine::Add(new)] if context == "context" && old == "old" && new == "new"))
        );
    }

    #[test]
    fn preflight_retains_the_source_and_destination_of_a_move() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("old"), "content").unwrap();
        let planned = plan("*** Update File: old\n*** Move to: new", &root).unwrap();
        let op = &planned.ops[0];
        assert_eq!(op.target.path, root.join("old"));
        assert_eq!(op.move_to.as_ref().unwrap().path, root.join("new"));
        assert_eq!(op.before.as_deref(), Some("content"));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_or_moving_a_link_leaves_its_target_intact() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "original\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for body in [
            "*** Delete File: link",
            "*** Update File: link\n*** Move to: moved",
        ] {
            let error = plan(body, dir.path()).unwrap_err().to_string();
            assert!(error.contains("symbolic link"), "{error}");
            assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
            assert!(link.is_symlink());
        }
    }

    #[test]
    fn case_aliases_on_a_case_insensitive_volume_are_repeated_paths() {
        let dir = TempDir::new().unwrap();
        let probe = dir.path().join("case-probe");
        fs::write(&probe, "").unwrap();
        if !dir.path().join("CASE-PROBE").exists() {
            return;
        }
        let error = plan(
            "*** Add File: nested/New\n+first\n*** Add File: NESTED/new\n+second",
            dir.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("repeated path"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_partial_move_reports_its_written_destination() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("a"), "before\n").unwrap();
        let saved_permissions = fs::metadata(&source_dir).unwrap().permissions();
        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = crate::registry::tool_registry_for_session(None, &[], true).execute(
            &request("*** Update File: source/a\n*** Move to: dest\n@@\n-before\n+after"),
            &crate::ToolContext::new(dir.path()),
        );
        fs::set_permissions(&source_dir, saved_permissions).unwrap();
        if !source_dir.join("a").exists() {
            return;
        }
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
        assert_eq!(
            fs::read_to_string(dir.path().join("dest")).unwrap(),
            "after\n"
        );
        assert!(result.output.unwrap().contains("Partially written: dest"));
        let preview = result.file_change_preview.unwrap().display_text();
        assert!(preview.contains("+++ b/dest"));
        assert!(!preview.contains("--- a/source/a"));
    }

    #[test]
    fn rejects_a_patch_without_begin_or_end() {
        for input in [
            "*** Add File: a\n+x\n*** End Patch",
            "*** Begin Patch\n*** Add File: a\n+x",
        ] {
            assert!(parse_patch(input).is_err());
        }
    }

    #[test]
    fn a_path_used_twice_in_one_patch_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("b"), "old\n").unwrap();
        for body in [
            "*** Add File: a\n+x\n*** Update File: ./a\n@@\n-x\n+y",
            "*** Add File: a\n+x\n*** Update File: b\n*** Move to: a\n@@\n-old\n+new",
            "*** Update File: b\n*** Move to: ./b\n@@\n-old\n+new",
        ] {
            assert!(
                plan(body, dir.path())
                    .unwrap_err()
                    .to_string()
                    .contains("repeated path")
            );
        }
    }

    #[test]
    fn an_outside_path_rejects_the_whole_patch() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("source"), "old\n").unwrap();
        for body in [
            "*** Add File: ../outside\n+bad",
            "*** Delete File: ../outside",
            "*** Update File: ../outside\n@@\n+x",
            "*** Update File: source\n*** Move to: ../outside\n@@\n+x",
        ] {
            let patch = format!("*** Add File: staged\n+x\n{body}");
            assert!(
                plan(&patch, dir.path())
                    .unwrap_err()
                    .to_string()
                    .contains("escapes workspace")
            );
            assert!(!dir.path().join("staged").exists());
        }
    }

    #[test]
    fn updating_a_missing_file_is_rejected() {
        let dir = TempDir::new().unwrap();
        let error = plan("*** Update File: missing\n@@\n-old\n+new", dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing") && error.contains("does not exist"));
    }

    #[test]
    fn adding_an_existing_file_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "original").unwrap();
        assert!(
            plan("*** Add File: a\n+new", dir.path())
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a")).unwrap(),
            "original"
        );
    }

    #[test]
    fn context_matching_falls_back_through_whitespace_levels() {
        let dir = TempDir::new().unwrap();
        for (input, old, expected) in [
            ("word\nword \n word\n", "word", "new\nword \n word\n"),
            ("word \n word\n", "word", "new\n word\n"),
            ("\tword \n", " word", "new\n"),
        ] {
            fs::write(dir.path().join("a"), input).unwrap();
            let planned =
                plan(&format!("*** Update File: a\n@@\n-{old}\n+new"), dir.path()).unwrap();
            assert_eq!(planned.ops[0].after.as_deref(), Some(expected));
        }
    }

    #[test]
    fn an_ambiguous_hunk_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "same\nsame\n").unwrap();
        assert!(
            plan("*** Update File: a\n@@\n-same\n+new", dir.path())
                .unwrap_err()
                .to_string()
                .contains("multiple locations")
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_refuses_a_parent_link_introduced_after_preparation() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/a"), "old\n").unwrap();
        fs::write(outside.path().join("a"), "old\n").unwrap();
        let prepared = [
            "*** Update File: sub/a\n@@\n-old\n+new",
            "*** Delete File: sub/a",
            "*** Add File: sub/b\n+new",
        ]
        .map(|body| (request(body), plan(body, dir.path()).unwrap()));
        fs::rename(dir.path().join("sub"), dir.path().join("saved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();
        for (req, plan) in prepared {
            let result = commit(&req, &plan, &|| false);
            assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
            assert_eq!(
                fs::read_to_string(outside.path().join("a")).unwrap(),
                "old\n"
            );
            assert!(!outside.path().join("b").exists());
        }
    }

    #[test]
    fn patch_refuses_replaced_or_edited_targets() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a");
        for replace in [false, true] {
            fs::write(&path, "old\n").unwrap();
            let body = "*** Update File: a\n@@\n-old\n+new";
            let prepared = plan(body, dir.path()).unwrap();
            let current = if replace {
                fs::rename(&path, dir.path().join("former")).unwrap();
                "old\n"
            } else {
                "edited\n"
            };
            fs::write(&path, current).unwrap();
            let result = commit(&request(body), &prepared, &|| false);
            assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
            assert_eq!(fs::read_to_string(&path).unwrap(), current);
        }
    }

    #[test]
    fn patch_refuses_hard_links_before_writing() {
        let dir = TempDir::new().unwrap();
        let original = dir.path().join("original");
        fs::write(&original, "old\n").unwrap();
        fs::hard_link(&original, dir.path().join("a")).unwrap();
        for body in ["*** Update File: a\n@@\n-old\n+new", "*** Delete File: a"] {
            let result = execute(&request(body), dir.path(), || false);
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("hard link"))
            );
            assert_eq!(fs::read_to_string(&original).unwrap(), "old\n");
            assert!(dir.path().join("a").exists());
        }
    }

    #[test]
    fn patch_rejects_equivalent_unicode_names_before_creating_either() {
        let dir = TempDir::new().unwrap();
        let result = execute(
            &request("*** Add File: café.txt\n+hi\n*** Add File: cafe\u{301}.txt\n+bye"),
            dir.path(),
            || false,
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("repeated path"))
        );
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
    }

    #[test]
    fn patch_reports_cancellation_before_an_operation_as_not_attempted() {
        let dir = TempDir::new().unwrap();
        let body = "*** Add File: a\n+x";
        let result = commit(&request(body), &plan(body, dir.path()).unwrap(), &|| true);
        let output = result.output.as_deref().unwrap();
        assert!(
            output.contains("Failed: (none)\nNot attempted: a"),
            "{output}"
        );
        assert!(!dir.path().join("a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn patch_reports_a_directory_created_before_a_failed_file() {
        let dir = TempDir::new().unwrap();
        let result = execute(
            &request(&format!("*** Add File: fresh/{}\n+x", "a".repeat(256))),
            dir.path(),
            || false,
        );
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap()
                .contains("created directory fresh")
        );
        assert!(dir.path().join("fresh").is_dir());
    }

    #[test]
    fn patch_preserves_an_explicit_blank_line() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "x").unwrap();
        let result = execute(
            &request("*** Update File: a\n@@\n-x\n+\n*** End of File"),
            dir.path(),
            || false,
        );
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"\n");
    }

    #[test]
    fn patch_rejects_context_only_hunks() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "context\r\nlast\n").unwrap();
        let result = execute(
            &request("*** Update File: a\n@@\n context"),
            dir.path(),
            || false,
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("no additions or removals")
        );
        assert_eq!(
            fs::read(dir.path().join("a")).unwrap(),
            b"context\r\nlast\n"
        );
    }

    #[test]
    fn patch_rejects_a_forward_whitespace_match_after_an_exact_match() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "x\nmarker\n  x\n").unwrap();
        let error = plan(
            "*** Update File: a\n@@\n-marker\n+new\n@@\n-x\n+replacement",
            dir.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exact match before"));
    }

    #[test]
    fn patch_limits_input_and_operation_count() {
        let dir = TempDir::new().unwrap();
        let oversized = request(&format!("*** Add File: a\n+{}", "a".repeat(1024 * 1024)));
        let too_many = request(
            &(0..101)
                .map(|i| format!("*** Add File: a{i}\n+x"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        for (req, message) in [(oversized, "1 MiB"), (too_many, "100 operations")] {
            let result = execute(&req, dir.path(), || false);
            assert!(result.error.as_deref().unwrap().contains(message));
            assert_eq!(dir.path().read_dir().unwrap().count(), 0);
        }
    }

    #[test]
    fn patch_limits_retained_text_before_writing() {
        let dir = TempDir::new().unwrap();
        let mut body = String::from("*** Add File: staged\n+x");
        let text = format!("first\n{}", "x".repeat(12 * 1024 * 1024));
        for file in ["a", "b", "c"] {
            fs::write(dir.path().join(file), &text).unwrap();
            body.push_str(&format!("\n*** Update File: {file}\n@@\n-first\n+new"));
        }
        let result = execute(&request(&body), dir.path(), || false);
        assert!(result.error.as_deref().unwrap().contains("64 MiB"));
        assert!(!dir.path().join("staged").exists());
    }

    #[test]
    fn patch_limits_matching_work_before_writing() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "abc\n".repeat(50_000)).unwrap();
        let body = format!(
            "*** Add File: staged\n+x\n*** Update File: a\n@@\n{}-never\n+new",
            " abc\n".repeat(500)
        );
        let result = execute(&request(&body), dir.path(), || false);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("matching work limit")
        );
        assert!(!dir.path().join("staged").exists());
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), "first\r\nsecond\r\nlast\n").unwrap();
        let planned = plan("*** Update File: a\n@@\n-second\n+new", dir.path()).unwrap();
        assert_eq!(
            planned.ops[0].after.as_deref(),
            Some("first\r\nnew\r\nlast\r\n")
        );
    }
}
