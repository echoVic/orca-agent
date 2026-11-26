use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::file_admission::{MAX_EDIT_FILE_BYTES, read_text_file_with_limit};
use crate::resolve_workspace_path;

#[derive(Debug)]
pub(crate) struct PatchError(String);

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
    let end_of_file = body.next_if_eq(&"*** End of File").is_some();
    Ok(Hunk {
        context_hint,
        lines,
        end_of_file,
    })
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) ops: Vec<PlannedOp>,
}

#[derive(Debug)]
pub(crate) struct PlannedTarget {
    pub(crate) path: PathBuf,
    pub(crate) label: String,
}

#[derive(Debug)]
pub(crate) struct PlannedOp {
    pub(crate) target: PlannedTarget,
    pub(crate) move_to: Option<PlannedTarget>,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
}

pub(crate) fn preflight(patch: &Patch, cwd: &Path) -> Result<Plan, PatchError> {
    let root = cwd
        .canonicalize()
        .map_err(|error| PatchError(format!("cannot resolve workspace: {error}")))?;
    let mut seen = HashSet::new();
    let mut ops = Vec::with_capacity(patch.ops.len());
    for (index, op) in patch.ops.iter().enumerate() {
        let (name, path, destination) = match op {
            PatchOp::Add { path, .. } => ("add", path, None),
            PatchOp::Delete { path } => ("delete", path, None),
            PatchOp::Update { path, move_to, .. } => ("update", path, move_to.as_deref()),
        };
        let planned = (|| {
            let target = resolve_target(cwd, &root, path, &mut seen)?;
            let move_to = destination
                .map(|dest| resolve_target(cwd, &root, dest, &mut seen))
                .transpose()?;
            if let Some(dest) = &move_to {
                require_missing(dest)?;
            }
            let (before, after) = match op {
                PatchOp::Add { lines, .. } => {
                    require_missing(&target)?;
                    let mut text = lines.join("\n");
                    if !lines.is_empty() {
                        text.push('\n');
                    }
                    (None, Some(text))
                }
                PatchOp::Delete { .. } => (Some(read_existing(&target)?), None),
                PatchOp::Update { hunks, .. } => {
                    let before = read_existing(&target)?;
                    let after = apply_hunks(&before, hunks)?;
                    (Some(before), Some(after))
                }
            };
            if let Some(after) = &after
                && after.len() > MAX_EDIT_FILE_BYTES
            {
                return Err(format!("result exceeds {MAX_EDIT_FILE_BYTES} bytes"));
            }
            Ok(PlannedOp {
                target,
                move_to,
                before,
                after,
            })
        })()
        .map_err(|reason: String| {
            PatchError(format!("operation {} ({name} {path}): {reason}", index + 1))
        })?;
        ops.push(planned);
    }
    Ok(Plan { ops })
}

fn resolve_target(
    cwd: &Path,
    root: &Path,
    label: &str,
    seen: &mut HashSet<PathBuf>,
) -> Result<PlannedTarget, String> {
    let path = resolve_workspace_path(cwd, Some(label))?;
    let mut ancestor = path.as_path();
    let canonical = loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                let canonical = ancestor
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve {label}: {error}"))?;
                if !canonical.starts_with(root) {
                    return Err(format!("path escapes workspace: {label}"));
                }
                if ancestor == path {
                    break canonical;
                }
                if !metadata.is_dir() && !canonical.is_dir() {
                    return Err(format!("a parent of {label} is not a directory"));
                }
                break canonical.join(
                    path.strip_prefix(ancestor)
                        .expect("ancestor of resolved path"),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| format!("cannot resolve {label}: {error}"))?;
            }
            Err(error) => return Err(format!("cannot resolve {label}: {error}")),
        }
    };
    if seen.contains(&canonical) {
        return Err(format!("repeated path: {label}"));
    }
    if seen
        .iter()
        .any(|previous| previous.starts_with(&canonical) || canonical.starts_with(previous))
    {
        return Err(format!(
            "file path conflicts with another operation: {label}"
        ));
    }
    seen.insert(canonical.clone());
    Ok(PlannedTarget {
        path: canonical,
        label: label.to_owned(),
    })
}

fn require_missing(target: &PlannedTarget) -> Result<(), String> {
    match target.path.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => Err(format!("{} already exists", target.label)),
        Err(error) => Err(format!("cannot check {}: {error}", target.label)),
    }
}

fn read_existing(target: &PlannedTarget) -> Result<String, String> {
    read_text_file_with_limit(&target.path, MAX_EDIT_FILE_BYTES, || false).map_err(|error| {
        if error.is_not_found() {
            format!("{} does not exist", target.label)
        } else {
            format!("cannot read {}: {error}", target.label)
        }
    })
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    if hunks.is_empty() {
        return Ok(original.to_owned());
    }
    let source: Vec<_> = original.lines().collect();
    let mut result = Vec::new();
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let start = if let Some(hint) = &hunk.context_hint {
            find_match(&source[cursor..], &[hint.as_str()], false)
                .map_err(|reason| format!("hunk {} hint: {reason}", index + 1))?
                + cursor
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
            find_match(&source[start..], &old, hunk.end_of_file)
                .map_err(|reason| format!("hunk {}: {reason}", index + 1))?
                + start
        };
        result.extend_from_slice(&source[cursor..offset]);
        cursor = offset;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    result.push(source[cursor]);
                    cursor += 1;
                }
                HunkLine::Remove(_) => cursor += 1,
                HunkLine::Add(text) => result.push(text),
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
    if !result.is_empty() && (original.is_empty() || original.ends_with('\n')) {
        updated.push_str(ending);
    }
    Ok(updated)
}

fn find_match(source: &[&str], old: &[&str], end_of_file: bool) -> Result<usize, &'static str> {
    let last = source
        .len()
        .checked_sub(old.len())
        .ok_or("context was not found")?;
    let first = if end_of_file { last } else { 0 };
    for normalize in [|line| line, str::trim_end, str::trim] as [fn(&str) -> &str; 3] {
        let mut matches = (first..=last).filter(|&offset| {
            source[offset..offset + old.len()]
                .iter()
                .zip(old)
                .all(|(actual, expected)| normalize(actual) == normalize(expected))
        });
        if let Some(offset) = matches.next() {
            return if matches.next().is_some() {
                Err("context matched multiple locations")
            } else {
                Ok(offset)
            };
        }
    }
    Err("context was not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn plan(body: &str, cwd: &Path) -> Result<Plan, PatchError> {
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
