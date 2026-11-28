use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use memchr::memchr;
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use orca_platform::fs::{DirectoryEntryKind, VerifiedDirectory};
use rayon::prelude::*;
use regex::bytes::Regex;
use regex_syntax::hir::{Hir, HirKind};
use serde::Deserialize;

use crate::ResolvedWorkspacePath;
use crate::spill::ReadTarget;
use crate::workspace_ignore::{
    IgnoreDirectory, IgnoreReads, WorkspaceIgnores, global_excludes_path,
};
use crate::workspace_read::{open_directory, open_file_in};

const DEFAULT_GREP_HEAD_LIMIT: usize = 250;
const MAX_GREP_HEAD_LIMIT: usize = 1_000;
const MIN_GREP_LINE_BYTES: usize = 8 * 1024;
const MAX_GREP_DISPLAY_BYTES: usize = 1024 * 1024;
const MAX_GREP_LINE_BYTES: usize = 16 * 1024 * 1024;
const GREP_TIMEOUT: Duration = Duration::from_secs(120);
const GREP_CANCELLED: &str = "grep was cancelled";

#[derive(Default, Deserialize)]
struct GrepArgs {
    pattern: Option<String>,
    path: Option<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, cwd, max_bytes, || false)
}

/// Like [`execute`], but stops the search once `should_cancel` says so.
pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_in_session(request, cwd, max_bytes, None, should_cancel)
}

pub(crate) fn execute_in_session(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    spill_dir: Option<&Path>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_search(request, cwd, max_bytes, spill_dir, &should_cancel, |stop| {
        global_excludes_path(cwd, stop)
    })
}

fn execute_search(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    spill_dir: Option<&Path>,
    should_cancel: &dyn Fn() -> bool,
    global_excludes: impl FnOnce(&IgnoreReads<'_>) -> Option<PathBuf>,
) -> ToolResult {
    let started = Instant::now();
    let cancelled = Cell::new(false);
    let is_cancelled = || {
        let observed = cancelled.get() || should_cancel();
        cancelled.set(observed);
        observed
    };
    let stop = || is_cancelled() || started.elapsed() >= GREP_TIMEOUT;
    if stop() {
        return ToolResult::cancelled(request, GREP_CANCELLED, None);
    }
    let args = parse_args(request);
    let pattern = args.pattern.as_deref().or(request
        .target
        .as_deref()
        .filter(|target| !target.is_empty()));
    let Some(pattern) = pattern else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };
    let regex = match compile_pattern(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let requested = args.path.as_deref().unwrap_or(".");
    let target = match crate::spill::resolve_read_path(cwd, Some(requested), spill_dir) {
        Ok(target) => target,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if let ReadTarget::Workspace(workspace) = &target
        && [&workspace.path, &workspace.canonical]
            .into_iter()
            .any(|path| {
                path.strip_prefix(&workspace.root)
                    .is_ok_and(|relative| relative.iter().any(is_git_directory))
            })
    {
        return render_search(request, GrepPageCollector::new(0, 0, 0), max_bytes);
    }
    let mut page = GrepPageCollector::new(
        args.offset.unwrap_or(0),
        normalized_head_limit(args.head_limit),
        max_bytes.max(1),
    );
    let max_line_bytes = max_bytes.clamp(MIN_GREP_LINE_BYTES, MAX_GREP_DISPLAY_BYTES);
    let result = match target {
        ReadTarget::Spill(_, file) => {
            search_file(file, None, &regex, max_line_bytes, &mut page, &stop, true)
        }
        ReadTarget::Workspace(workspace) => {
            let label = if Path::new(requested).is_absolute() {
                workspace.path.as_path()
            } else {
                workspace.path.strip_prefix(cwd).unwrap_or(&workspace.path)
            };
            let search = || match open_directory(&workspace.root, &workspace.canonical) {
                Ok(directory) if !stop() => {
                    let reads = IgnoreReads::new(&stop);
                    let global = global_excludes(&reads);
                    let ignores = WorkspaceIgnores::new(
                        &workspace.root,
                        &workspace.canonical,
                        global.as_deref(),
                        &reads,
                    );
                    DirectorySearch {
                        workspace: &workspace,
                        label,
                        regex: &regex,
                        max_line_bytes,
                        ignores: &ignores,
                    }
                    .search(&directory, &mut page, &reads)
                }
                Ok(_) => Ok(()),
                Err(directory_error) => match crate::workspace_read::open_read_file(&workspace) {
                    Ok((file, _)) => search_file(
                        file,
                        Some(label),
                        &regex,
                        max_line_bytes,
                        &mut page,
                        &stop,
                        true,
                    ),
                    Err(error)
                        if error.kind() == io::ErrorKind::NotFound
                            && directory_error.kind() == io::ErrorKind::NotFound =>
                    {
                        Ok(())
                    }
                    Err(_) if is_resource_failure(&directory_error) => Err(directory_error),
                    Err(error) => Err(error),
                },
            };
            search().and_then(|()| crate::workspace_read::open_root(&workspace.root).map(drop))
        }
    };
    if is_cancelled() {
        return ToolResult::cancelled(request, GREP_CANCELLED, None);
    }
    if started.elapsed() >= GREP_TIMEOUT {
        return ToolResult::failed(request, "grep timed out after 120s", None);
    }
    if let Err(error) = result {
        return ToolResult::failed(request, error.to_string(), None);
    }
    render_search(request, page, max_bytes)
}

fn compile_pattern(pattern: &str) -> Result<Regex, String> {
    if !pattern.contains(['\\', '\n', '\0']) {
        return Regex::new(pattern).map_err(|error| error.to_string());
    }
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(pattern)
        .map_err(|error| error.to_string())?;
    if contains_literal(&hir, b'\n') {
        return Err(format!(
            "the pattern can match a line break, which grep does not support: {pattern}"
        ));
    }
    if contains_literal(&hir, 0) {
        return Err("pattern contains a NUL byte but it is impossible to match".to_string());
    }
    Regex::new(pattern).map_err(|error| error.to_string())
}

// Classes such as whitespace also match ordinary line content. As in ripgrep,
// the line-break error is reserved for literal terminators in the compiled pattern.
fn contains_literal(hir: &Hir, byte: u8) -> bool {
    match hir.kind() {
        HirKind::Literal(literal) => literal.0.contains(&byte),
        kind => kind
            .subs()
            .iter()
            .any(|child| contains_literal(child, byte)),
    }
}

fn is_git_directory(name: &std::ffi::OsStr) -> bool {
    if cfg!(windows) {
        name.as_encoded_bytes().eq_ignore_ascii_case(b".git")
    } else {
        name == ".git"
    }
}

struct DirectorySearch<'a> {
    workspace: &'a ResolvedWorkspacePath,
    label: &'a Path,
    regex: &'a Regex,
    max_line_bytes: usize,
    ignores: &'a WorkspaceIgnores<'a>,
}

impl DirectorySearch<'_> {
    fn search(
        &self,
        directory: &VerifiedDirectory,
        page: &mut GrepPageCollector,
        reads: &IgnoreReads<'_>,
    ) -> io::Result<()> {
        reads.check()?;
        let should_stop = reads.stop;
        // Keep only paths during discovery. Ordering happens as each entry is
        // inserted, so cancellation can interrupt it even in a flat directory.
        let mut files = BTreeSet::new();
        let result = self.walk(directory, Path::new(""), None, &mut files, reads);
        if should_stop() {
            return Ok(());
        }
        result?;
        let files = files.into_iter().map(PathBuf::from);
        // Only the calling thread invokes the caller's cancellation callback.
        let stop = AtomicBool::new(false);
        let stop_ref = &stop;
        std::thread::scope(|scope| {
            let (done, receive) = mpsc::sync_channel(1);
            scope.spawn(move || {
                static POOL: OnceLock<Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>> =
                    OnceLock::new();
                let pool = POOL.get_or_init(|| {
                    let threads = std::thread::available_parallelism()
                        .map_or(2, usize::from)
                        .min(8);
                    rayon::ThreadPoolBuilder::new().num_threads(threads).build()
                });
                let should_stop = || stop_ref.load(Ordering::Relaxed);
                let result = match pool {
                    Ok(pool) => {
                        pool.install(|| self.search_directory(directory, files, page, &should_stop))
                    }
                    Err(error) => Err(io::Error::other(error.to_string())),
                };
                let _ = done.send(result);
            });
            loop {
                if should_stop() {
                    stop.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                match receive.recv_timeout(Duration::from_millis(5)) {
                    Ok(result) => return result,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::other("grep worker stopped"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        })
    }

    fn search_directory(
        &self,
        directory: &VerifiedDirectory,
        mut files: impl Iterator<Item = PathBuf>,
        page: &mut GrepPageCollector,
        should_stop: &(impl Fn() -> bool + Sync),
    ) -> io::Result<()> {
        while !should_stop() && (page.offset != 0 || !page.has_next_page()) {
            let batch = files.by_ref().take(128).collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            if page.offset != 0 {
                self.search_files(directory, &batch, self.regex, page, should_stop)?;
                continue;
            }
            let chunks = batch
                .par_chunks(16)
                .map(|chunk| {
                    let mut first = GrepPageCollector::new(0, page.limit, page.retained_budget);
                    self.search_files(
                        directory,
                        chunk,
                        &self.regex.clone(),
                        &mut first,
                        should_stop,
                    )?;
                    Ok(first)
                })
                .collect::<Vec<io::Result<_>>>();
            for chunk in chunks {
                page.append_first_page(chunk?);
            }
        }
        Ok(())
    }

    fn walk(
        &self,
        base: &VerifiedDirectory,
        relative: &Path,
        parents: Option<&IgnoreDirectory<'_>>,
        files: &mut BTreeSet<OsString>,
        reads: &IgnoreReads<'_>,
    ) -> io::Result<()> {
        let should_stop = reads.stop;
        if should_stop() {
            return Ok(());
        }
        let opened = open_search_directory(base, relative)?;
        if !relative.as_os_str().is_empty() && opened.is_none() {
            return Ok(());
        }
        let directory = opened.as_ref().unwrap_or(base);
        let path = self.workspace.canonical.join(relative);
        let ignores = IgnoreDirectory {
            rules: self.ignores.directory(&path, directory, reads),
            parent: parents,
        };
        reads.check()?;
        let mut folders = Vec::new();
        let entries = directory.visit_entries(|entry| {
            if should_stop() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if is_git_directory(&entry.name) || entry.kind == DirectoryEntryKind::Other {
                return Ok(());
            }
            let is_dir = entry.kind == DirectoryEntryKind::Directory;
            if self
                .ignores
                .entry_ignored(&path.join(&entry.name), is_dir, entry.hidden, &ignores)
            {
                return Ok(());
            }
            let child = relative.join(entry.name);
            if is_dir {
                folders.push(child);
            } else {
                files.insert(child.into_os_string());
            }
            Ok(())
        });
        if let Err(error) = entries {
            if should_stop() {
                return Ok(());
            }
            return Err(error);
        }
        drop(opened);
        for child in folders {
            self.walk(base, &child, Some(&ignores), files, reads)?;
        }
        Ok(())
    }

    fn search_files(
        &self,
        base: &VerifiedDirectory,
        files: &[PathBuf],
        regex: &Regex,
        page: &mut GrepPageCollector,
        should_stop: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        let mut parent = None;
        let mut opened = None;
        for path in files {
            if should_stop() || (page.offset == 0 && page.has_next_page()) {
                break;
            }
            let current = path.parent().expect("relative file path");
            if parent != Some(current) {
                parent = Some(current);
                opened = open_search_directory(base, current)?;
            }
            let directory = if current.as_os_str().is_empty() {
                Some(base)
            } else {
                opened.as_ref()
            };
            if let Some(directory) = directory {
                let found = open_file_in(directory, path.file_name().expect("file name")).and_then(
                    |file| {
                        let label = self.label.join(path);
                        search_file(
                            file,
                            Some(&label),
                            regex,
                            self.max_line_bytes,
                            page,
                            should_stop,
                            false,
                        )
                    },
                );
                if let Err(error) = found
                    && is_resource_failure(&error)
                {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

fn open_search_directory(
    base: &VerifiedDirectory,
    relative: &Path,
) -> io::Result<Option<VerifiedDirectory>> {
    let mut directory = None;
    for component in relative {
        match directory.as_ref().unwrap_or(base).child_for_read(component) {
            Ok(child) => directory = Some(child),
            Err(error) if is_resource_failure(&error) => return Err(error),
            Err(_) => return Ok(None),
        }
    }
    Ok(directory)
}

pub(crate) fn is_resource_failure(error: &io::Error) -> bool {
    #[cfg(unix)]
    return matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS)
    );
    #[cfg(windows)]
    return matches!(error.raw_os_error(), Some(4 | 8 | 14 | 1450));
}

fn search_file(
    file: File,
    path: Option<&Path>,
    regex: &Regex,
    max_line_bytes: usize,
    page: &mut GrepPageCollector,
    should_stop: &dyn Fn() -> bool,
    explicit: bool,
) -> io::Result<()> {
    let saved = (
        page.total_lines,
        page.lines.len(),
        page.retained_bytes,
        page.selection_truncated,
        page.skipped_lines,
    );
    let mut reader = BufReader::with_capacity(32 * 1024, file);
    let prefix = reader.fill_buf()?;
    let encoding = if prefix.starts_with(b"\xef\xbb\xbf") {
        reader.consume(3);
        None
    } else if prefix.starts_with(b"\xff\xfe") {
        reader.consume(2);
        Some(false)
    } else if prefix.starts_with(b"\xfe\xff") {
        reader.consume(2);
        Some(true)
    } else {
        None
    };
    let path = path.map(Path::to_string_lossy);
    #[cfg(windows)]
    let path = path.map(|path| path.replace('\\', "/"));
    let mut visit = |number, line: &[u8], length| {
        if length > MAX_GREP_LINE_BYTES {
            page.skipped_lines += 1;
        } else if regex.is_match(line) {
            page.push(path.as_deref(), number, line, length, max_line_bytes);
        }
    };
    let read = match encoding {
        Some(big_endian) => scan_utf16(reader, big_endian, should_stop, explicit, &mut visit),
        None => scan_bytes(reader, should_stop, explicit, &mut visit),
    };
    let binary_match =
        explicit && matches!(read, Ok(ScanEnd::Binary)) && page.total_lines > saved.0;
    if !matches!(read, Ok(ScanEnd::Text)) {
        (
            page.total_lines,
            _,
            page.retained_bytes,
            page.selection_truncated,
            page.skipped_lines,
        ) = saved;
        page.lines.truncate(saved.1);
    }
    if binary_match {
        let text = path.map_or_else(
            || "binary file matches".to_string(),
            |path| format!("{path}: binary file matches"),
        );
        page.total_lines += 1;
        if saved.0 >= page.offset && saved.0 - page.offset < page.limit {
            page.retain(StoredLine {
                text,
                truncated: false,
            });
        }
    }
    read.map(drop)
}

enum ScanEnd {
    Text,
    Binary,
    Stopped,
}

fn scan_bytes(
    mut reader: impl BufRead,
    should_stop: &dyn Fn() -> bool,
    explicit: bool,
    visit: &mut impl FnMut(usize, &[u8], usize),
) -> io::Result<ScanEnd> {
    let mut line = SearchLine::default();
    let mut binary = false;
    while !should_stop() {
        let data = reader.fill_buf()?;
        if data.is_empty() {
            if line.length != 0 {
                line.finish(b"", false, visit);
            }
            return Ok(if binary {
                ScanEnd::Binary
            } else {
                ScanEnd::Text
            });
        }
        binary |= memchr(0, data).is_some();
        if binary && !explicit {
            return Ok(ScanEnd::Binary);
        }
        let mut start = 0;
        let breaks = if binary {
            memchr::memchr2_iter(b'\n', 0, data)
        } else {
            memchr::memchr2_iter(b'\n', b'\n', data)
        };
        for end in breaks {
            line.finish(&data[start..end], true, visit);
            start = end + 1;
        }
        line.extend(&data[start..]);
        let consumed = data.len();
        reader.consume(consumed);
    }
    Ok(ScanEnd::Stopped)
}

fn scan_utf16(
    mut reader: impl BufRead,
    big_endian: bool,
    should_stop: &dyn Fn() -> bool,
    explicit: bool,
    visit: &mut impl FnMut(usize, &[u8], usize),
) -> io::Result<ScanEnd> {
    let mut line = SearchLine::default();
    let mut binary = false;
    let mut error = None;
    let units = std::iter::from_fn(|| {
        let mut unit = [0; 2];
        match reader.read(&mut unit[..1]) {
            Ok(0) => return None,
            Ok(_) => match reader.read_exact(&mut unit[1..]) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Some(0xfffd),
                Err(err) => {
                    error = Some(err);
                    return None;
                }
            },
            Err(err) => {
                error = Some(err);
                return None;
            }
        }
        Some(if big_endian {
            u16::from_be_bytes(unit)
        } else {
            u16::from_le_bytes(unit)
        })
    });
    for (index, scalar) in char::decode_utf16(units).enumerate() {
        if index % 1024 == 0 && should_stop() {
            return Ok(ScanEnd::Stopped);
        }
        match scalar.unwrap_or(char::REPLACEMENT_CHARACTER) {
            '\0' if !explicit => return Ok(ScanEnd::Binary),
            '\0' => {
                binary = true;
                line.finish(b"", true, visit);
            }
            '\n' => line.finish(b"", true, visit),
            ch => line.extend(ch.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    if line.length != 0 {
        line.finish(b"", false, visit);
    }
    Ok(if binary {
        ScanEnd::Binary
    } else {
        ScanEnd::Text
    })
}

#[derive(Default)]
struct SearchLine {
    number: usize,
    length: usize,
    bytes: Vec<u8>,
    last: Option<u8>,
}

impl SearchLine {
    fn extend(&mut self, bytes: &[u8]) {
        // Leave room for the CR in a max-sized CRLF line.
        let keep = bytes
            .len()
            .min((MAX_GREP_LINE_BYTES + 1).saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..keep]);
        self.length = self.length.saturating_add(bytes.len());
        self.last = bytes.last().copied().or(self.last);
    }

    fn finish(&mut self, tail: &[u8], newline: bool, visit: &mut impl FnMut(usize, &[u8], usize)) {
        self.number += 1;
        if self.length == 0 {
            let text = if newline {
                tail.strip_suffix(b"\r").unwrap_or(tail)
            } else {
                tail
            };
            visit(self.number, text, text.len());
        } else {
            self.extend(tail);
            if newline && self.last == Some(b'\r') {
                if self.length == self.bytes.len() {
                    self.bytes.pop();
                }
                self.length -= 1;
            }
            visit(self.number, &self.bytes, self.length);
            self.bytes.clear();
            self.length = 0;
            self.last = None;
        }
    }
}

fn render_search(
    request: &ToolRequest,
    collector: GrepPageCollector,
    max_bytes: usize,
) -> ToolResult {
    let no_matches = collector.total_lines == 0;
    let skipped_lines = collector.skipped_lines;
    let (mut output, truncated) = if no_matches {
        ("(no matches)".to_string(), false)
    } else {
        collector.render(max_bytes)
    };
    if skipped_lines > 0 {
        output.push_str(&format!(
            "\n{skipped_lines} lines longer than 16 MiB were not searched"
        ));
    }
    ToolResult::completed_kind(
        request,
        output,
        truncated || skipped_lines > 0,
        if no_matches {
            ToolResultKind::NoMatches
        } else if truncated || skipped_lines > 0 {
            ToolResultKind::Truncated
        } else {
            ToolResultKind::Success
        },
    )
}

fn parse_args(request: &ToolRequest) -> GrepArgs {
    request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<GrepArgs>(raw).ok())
        .unwrap_or_default()
}

fn normalized_head_limit(head_limit: Option<usize>) -> usize {
    match head_limit {
        None | Some(0) => DEFAULT_GREP_HEAD_LIMIT,
        Some(limit) => limit.min(MAX_GREP_HEAD_LIMIT),
    }
}

struct GrepPageCollector {
    offset: usize,
    limit: usize,
    total_lines: usize,
    retained_bytes: usize,
    retained_budget: usize,
    lines: Vec<StoredLine>,
    selection_truncated: bool,
    skipped_lines: usize,
}

struct StoredLine {
    text: String,
    truncated: bool,
}

impl GrepPageCollector {
    fn new(offset: usize, limit: usize, retained_budget: usize) -> Self {
        Self {
            offset,
            limit,
            total_lines: 0,
            retained_bytes: 0,
            retained_budget,
            lines: Vec::new(),
            selection_truncated: false,
            skipped_lines: 0,
        }
    }

    fn has_next_page(&self) -> bool {
        self.total_lines.saturating_sub(self.offset) > self.limit
    }

    fn push(
        &mut self,
        path: Option<&str>,
        number: usize,
        line: &[u8],
        length: usize,
        max_line_bytes: usize,
    ) {
        let index = self.total_lines;
        self.total_lines = self.total_lines.saturating_add(1);
        if index < self.offset || index - self.offset >= self.limit {
            return;
        }
        let mut keep = line.len().min(max_line_bytes);
        while keep < line.len() && keep > 0 && line[keep] & 0xc0 == 0x80 {
            keep -= 1;
        }
        let omitted = length.saturating_sub(keep);
        let line = String::from_utf8_lossy(&line[..keep]);
        let text = match path {
            Some(path) => format!("{path}:{number}:{line}"),
            None => format!("{number}:{line}"),
        };
        let text = crate::process::preserve_ingress_omission_notice(text, omitted);
        self.retain(StoredLine {
            text,
            truncated: omitted > 0,
        });
    }

    // Retain original records through the first one that exceeds the page budget.
    // A worker's budget starts at zero, so the final page can only need fewer.
    fn retain(&mut self, line: StoredLine) {
        let separator = usize::from(!self.lines.is_empty());
        if self.retained_bytes.saturating_add(separator) >= self.retained_budget {
            self.selection_truncated = true;
            return;
        }
        self.retained_bytes += separator + line.text.len();
        self.lines.push(line);
    }

    fn append_first_page(&mut self, page: Self) {
        debug_assert_eq!(self.offset, 0);
        let selected = self
            .limit
            .saturating_sub(self.total_lines)
            .min(page.total_lines);
        self.selection_truncated |= selected > page.lines.len();
        self.total_lines = self.total_lines.saturating_add(page.total_lines);
        self.skipped_lines += page.skipped_lines;
        for line in page.lines.into_iter().take(selected) {
            self.retain(line);
        }
    }
    fn render(self, max_bytes: usize) -> (String, bool) {
        let next_offset = (self.total_lines.saturating_sub(self.offset) > self.limit)
            .then_some(self.offset.saturating_add(self.limit));
        let notice = next_offset.map(|next_offset| {
            if self.offset == 0 {
                format!(
                    "[Showing first {} results; use offset={next_offset} to continue]",
                    self.limit
                )
            } else {
                let end = next_offset.min(self.total_lines);
                format!(
                    "[Showing results {}-{end} of {}; use offset={next_offset} to continue]",
                    self.offset + 1,
                    self.total_lines
                )
            }
        });
        let available = max_bytes.saturating_sub(notice.as_ref().map_or(0, |text| text.len() + 1));
        let mut output = String::new();
        let mut truncated = self.selection_truncated;
        for line in self.lines {
            let separator = usize::from(!output.is_empty());
            let remaining = available.saturating_sub(output.len() + separator);
            if remaining == 0 {
                truncated = true;
                break;
            }
            let (text, shortened) = truncate_output(line.text, remaining);
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&text);
            truncated |= line.truncated || shortened;
            if shortened {
                break;
            }
        }
        if let Some(notice) = notice {
            if !output.is_empty() {
                output.push('\n');
            }
            let (notice, shortened) =
                truncate_output(notice, max_bytes.saturating_sub(output.len()));
            truncated |= shortened;
            output.push_str(&notice);
        }
        (output, truncated)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[test]
    fn grep_matches_the_whole_line_without_matching_a_cut() {
        let workspace = crate::workspace_tempdir().unwrap();
        for (pattern, text, matched) in [
            (
                "needle",
                format!("{}needle\n", "x".repeat(1024 * 1024)),
                true,
            ),
            ("x$", format!("{}needle\n", "x".repeat(1024 * 1024)), false),
            (
                "�",
                format!("{}界needle\n", "x".repeat(1024 * 1024 - 1)),
                false,
            ),
        ] {
            fs::write(workspace.path().join("long.txt"), text).unwrap();
            let request = grep_request(&serde_json::json!({"pattern": pattern}).to_string());
            let result = execute(&request, workspace.path(), 4096);
            assert_eq!(
                result.output.as_deref() != Some("(no matches)"),
                matched,
                "{pattern}: {result:?}"
            );
        }
    }

    #[test]
    fn grep_notes_each_line_over_sixteen_mib_once() {
        let workspace = crate::workspace_tempdir().unwrap();
        let oversized = format!("needle{}\n", "x".repeat(16 * 1024 * 1024));
        for name in ["a.txt", "b.txt"] {
            fs::write(workspace.path().join(name), &oversized).unwrap();
        }
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, workspace.path(), 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("(no matches)\n2 lines longer than 16 MiB were not searched")
        );
    }

    #[test]
    fn grep_compacts_records_once_across_chunk_and_batch_boundaries() {
        for boundary in [16, 128] {
            let workspace = crate::workspace_tempdir().unwrap();
            let first = format!("needle{}", "a".repeat(47));
            let second = format!("needle{}", "b".repeat(120));
            for index in 0..=boundary {
                let content = match index {
                    0 => first.as_str(),
                    index if index == boundary => second.as_str(),
                    _ => "miss",
                };
                fs::write(workspace.path().join(format!("{index:03}.txt")), content).unwrap();
            }
            let request = grep_request(r#"{"pattern":"needle","head_limit":2}"#);
            let result = execute(&request, workspace.path(), 100);
            let expected = format!(
                "000.txt:1:{first}\n{boundary:03}.txt:1:needle{}",
                "b".repeat(20)
            );
            assert_eq!(result.output.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn grep_checks_cancellation_during_discovery() {
        let workspace = crate::workspace_tempdir().unwrap();
        for index in 0..256 {
            fs::write(workspace.path().join(format!("{index}.txt")), "miss").unwrap();
        }
        let polls = Cell::new(0);
        let result = execute_or_cancel(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
            || {
                polls.set(polls.get() + 1);
                polls.get() >= 32
            },
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
    }

    #[test]
    fn grep_skips_an_oversized_ignore_file() {
        let workspace = crate::workspace_tempdir().unwrap();
        let mut rules = String::from("notes.txt\n#");
        rules.push_str(&"x".repeat(1024 * 1024));
        fs::write(workspace.path().join(".ignore"), rules).unwrap();
        fs::write(workspace.path().join("notes.txt"), "needle").unwrap();
        let result = execute(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(result.output.as_deref(), Some("notes.txt:1:needle"));
    }

    #[cfg(unix)]
    #[test]
    fn grep_searches_deep_trees_with_a_low_descriptor_limit() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "ORCA_TEST_GREP_LOW_DESCRIPTOR_LIMIT";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "grep::tests::grep_searches_deep_trees_with_a_low_descriptor_limit",
                ])
                .env(CHILD, "1");
            unsafe {
                child.pre_exec(|| {
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    limit.rlim_cur = 48.min(limit.rlim_max);
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let workspace = crate::workspace_tempdir().unwrap();
        let leaf = workspace.path().join("level/".repeat(64));
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("notes.txt"), "needle").unwrap();
        let result = execute(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
        );
        assert!(
            result
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("notes.txt:1:needle"),
            "{result:?}"
        );
        let mut descriptors = Vec::new();
        while let Ok(file) = File::open(leaf.join("notes.txt")) {
            descriptors.push(file);
        }
        assert_eq!(
            execute(
                &grep_request(r#"{"pattern":"needle"}"#),
                workspace.path(),
                4096
            )
            .status,
            ToolStatus::Failed
        );
    }

    #[test]
    fn grep_uses_byte_regex_with_unicode_defaults() {
        let workspace = crate::workspace_tempdir().unwrap();
        fs::write(workspace.path().join("notes.txt"), b"needle\xff\n").unwrap();
        for (pattern, expected) in [
            (r"(?-u:\xFF)", "notes.txt:1:needle�"),
            ("�", "(no matches)"),
        ] {
            let request = grep_request(&serde_json::json!({"pattern": pattern}).to_string());
            let result = execute(&request, workspace.path(), 4096);
            assert_eq!(result.output.as_deref(), Some(expected), "{result:?}");
        }
    }

    #[test]
    fn grep_refuses_explicit_line_breaks_in_patterns() {
        let workspace = crate::workspace_tempdir().unwrap();
        for pattern in [r"needle\n", r"\x0a", "needle\n", r"[\n]"] {
            let request = grep_request(&serde_json::json!({"pattern": pattern}).to_string());
            let result = execute(&request, workspace.path(), 4096);
            assert_eq!(
                result.error.as_deref(),
                Some(
                    format!(
                        "the pattern can match a line break, which grep does not support: {pattern}"
                    )
                    .as_str()
                )
            );
        }
    }

    #[test]
    fn grep_admits_whitelisted_hidden_files_and_always_skips_dot_git() {
        let workspace = crate::workspace_tempdir().unwrap();
        let cwd = workspace.path();
        fs::create_dir(cwd.join(".git")).unwrap();
        fs::write(cwd.join(".git/notes.txt"), "needle\n").unwrap();
        fs::write(cwd.join(".env"), "needle\r\n").unwrap();
        fs::write(cwd.join(".hidden"), "needle\n").unwrap();
        fs::write(cwd.join(".ignore"), "!.env\n!.git/\n").unwrap();
        for (target, expected) in [
            (".", ".env:1:needle"),
            (".env", ".env:1:needle"),
            (".git/notes.txt", "(no matches)"),
        ] {
            let request = grep_request(
                &serde_json::json!({"pattern": "^needle$", "path": target}).to_string(),
            );
            assert_eq!(
                execute(&request, cwd, 4096).output.as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn grep_reports_explicit_binary_matches() {
        let workspace = crate::workspace_tempdir().unwrap();
        let cwd = workspace.path();
        fs::write(cwd.join("notes.bin"), b"needle\n\0\n").unwrap();
        let search = |path, pattern| {
            execute(
                &grep_request(&serde_json::json!({"pattern": pattern, "path": path}).to_string()),
                cwd,
                4096,
            )
        };
        assert_eq!(
            search("notes.bin", "needle").output.as_deref(),
            Some("notes.bin: binary file matches")
        );
        assert_eq!(
            search("notes.bin", "other").output.as_deref(),
            Some("(no matches)")
        );
        assert_eq!(
            search(".", "needle").output.as_deref(),
            Some("(no matches)")
        );
    }

    #[cfg(unix)]
    #[test]
    fn grep_reports_an_unreadable_explicit_file_and_skips_it_recursively() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = crate::workspace_tempdir().unwrap();
        let path = workspace.path().join("notes.txt");
        fs::write(&path, "needle").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&path).is_ok() {
            return;
        }
        let explicit = execute(
            &grep_request(r#"{"pattern":"needle","path":"notes.txt"}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(explicit.status, ToolStatus::Failed);
        assert!(explicit.error.as_deref().unwrap().contains("notes.txt"));
        let recursive = execute(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(recursive.output.as_deref(), Some("(no matches)"));
    }

    #[cfg(unix)]
    #[test]
    fn grep_preserves_literal_backslashes_in_unix_paths() {
        let workspace = crate::workspace_tempdir().unwrap();
        fs::write(workspace.path().join(r"one\two.txt"), "needle").unwrap();
        let result = execute(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(result.output.as_deref(), Some(r"one\two.txt:1:needle"));
    }

    #[test]
    fn grep_resolves_a_relative_global_excludes_file_from_the_workspace() {
        const CHILD: &str = "ORCA_TEST_GREP_GLOBAL_CONFIG";
        let workspace = crate::workspace_tempdir().unwrap();
        let cwd = workspace.path();
        if std::env::var_os(CHILD).is_none() {
            let config = cwd.join("gitconfig");
            fs::write(&config, "[core]\n  excludesFile = excludes\n").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "grep::tests::grep_resolves_a_relative_global_excludes_file_from_the_workspace",
                ])
                .env("GIT_CONFIG_GLOBAL", config)
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        fs::create_dir(cwd.join(".git")).unwrap();
        fs::write(cwd.join("excludes"), "notes.txt\n").unwrap();
        fs::write(cwd.join("notes.txt"), "needle").unwrap();
        let result = execute(&grep_request(r#"{"pattern":"needle"}"#), cwd, 4096);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
    }

    #[test]
    fn directory_search_keeps_matches_in_utf8_files_with_a_bom() {
        let workspace = crate::workspace_tempdir().unwrap();
        fs::write(
            workspace.path().join("bom.txt"),
            b"\xef\xbb\xbfneedle one\nneedle two\n",
        )
        .unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, workspace.path(), 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("bom.txt:1:needle one\nbom.txt:2:needle two")
        );
    }

    #[cfg(unix)]
    #[test]
    fn recursive_grep_keeps_native_filenames_in_the_page() {
        use std::os::unix::ffi::OsStrExt;

        let workspace = crate::workspace_tempdir().unwrap();
        for name in ["z.txt", "a\nfile.txt"] {
            fs::write(workspace.path().join(name), "needle\n").unwrap();
        }
        let native = workspace
            .path()
            .join(std::ffi::OsStr::from_bytes(b"b\xff.txt"));
        let (middle, name) = match fs::write(native, "needle\n") {
            Ok(()) => ("b�.txt", None),
            Err(_) if cfg!(target_os = "macos") => ("b-é.txt", Some("b-é.txt")),
            Err(error) => panic!("cannot create byte filename: {error}"),
        };
        if let Some(name) = name {
            fs::write(workspace.path().join(name), "needle\n").unwrap();
        }
        let result = execute(
            &grep_request(r#"{"pattern":"needle","offset":1,"head_limit":1}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(
            result.output.as_deref(),
            Some(
                format!("{middle}:1:needle\n[Showing results 2-2 of 3; use offset=2 to continue]")
                    .as_str()
            )
        );
        let result = execute(
            &grep_request(r#"{"pattern":"needle","head_limit":3}"#),
            workspace.path(),
            4096,
        );
        assert_eq!(
            result.output.as_deref(),
            Some(format!("a\nfile.txt:1:needle\n{middle}:1:needle\nz.txt:1:needle").as_str())
        );
    }

    #[test]
    fn recursive_grep_decodes_unicode_and_returns_a_sorted_page() {
        let workspace = crate::workspace_tempdir().unwrap();
        for (name, big_endian) in [("z-be.txt", true), ("a-le.txt", false)] {
            let mut bytes = if big_endian {
                vec![0xfe, 0xff]
            } else {
                vec![0xff, 0xfe]
            };
            for unit in "first line\r\nneedle 🌱\n".encode_utf16() {
                bytes.extend(if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                });
            }
            fs::write(workspace.path().join(name), bytes).unwrap();
        }
        fs::write(workspace.path().join("m.txt"), b"\xef\xbb\xbfneedle\n").unwrap();
        fs::write(
            workspace.path().join("b-binary.txt"),
            b"needle\nsecond line\0\n",
        )
        .unwrap();
        fs::write(workspace.path().join(".hidden.txt"), "needle\n").unwrap();
        let request = grep_request(r#"{"pattern":"^needle","offset":1,"head_limit":1}"#);
        for _ in 0..3 {
            let result = execute(&request, workspace.path(), 4096);
            assert_eq!(
                result.output.as_deref(),
                Some("m.txt:1:needle\n[Showing results 2-2 of 3; use offset=2 to continue]")
            );
        }
        let request = grep_request(r#"{"pattern":"^needle","head_limit":3}"#);
        let result = execute(&request, workspace.path(), 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("a-le.txt:2:needle 🌱\nm.txt:1:needle\nz-be.txt:2:needle 🌱")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_skipped_and_search_continues() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = crate::workspace_tempdir().unwrap();
        let unreadable = workspace.path().join("denied.txt");
        fs::write(&unreadable, "needle\n").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(workspace.path().join("visible.txt"), "needle\n").unwrap();
        let can_read_denied = File::open(&unreadable).is_ok();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, workspace.path(), 4096);
        assert_eq!(result.status, ToolStatus::Completed);
        let output = result.output.unwrap();
        assert_eq!(output.contains("denied.txt"), can_read_denied);
        assert!(output.contains("visible.txt:1:needle"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_ignore_file_pointing_outside_is_not_read() {
        let fixture = crate::workspace_tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::write(cwd.join("notes.txt"), "needle\n").unwrap();
        let outside = fixture.path().join("rules");
        fs::write(&outside, "notes.txt\n").unwrap();
        crate::tests::symlink(&outside, cwd.join(".gitignore")).unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, &cwd, 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("notes.txt:1:needle"),
            "{result:?}"
        );
    }

    #[test]
    fn the_global_excludes_file_still_filters() {
        let fixture = crate::workspace_tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::write(cwd.join("excluded.txt"), "needle\n").unwrap();
        fs::write(cwd.join("visible.txt"), "needle\n").unwrap();
        let excludes = fixture.path().join("global-excludes");
        fs::write(&excludes, "excluded.txt\n").unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute_search(&request, &cwd, 4096, None, &|| false, |_| {
            Some(excludes.clone())
        });
        assert_eq!(
            result.output.as_deref(),
            Some("visible.txt:1:needle"),
            "{result:?}"
        );
    }

    #[test]
    fn a_linked_worktrees_info_exclude_still_filters() {
        let fixture = crate::workspace_tempdir().unwrap();
        let cwd = fixture.path().join("linked");
        let common = fixture.path().join("repo/.git");
        let worktree = common.join("worktrees/linked");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir(common.join("info")).unwrap();
        fs::write(cwd.join(".git"), "gitdir: ../repo/.git/worktrees/linked\n").unwrap();
        fs::write(worktree.join("commondir"), "../..\n").unwrap();
        fs::write(worktree.join("gitdir"), "../../../../linked/.git\n").unwrap();
        fs::write(common.join("info/exclude"), "excluded.txt\n").unwrap();
        fs::write(cwd.join("excluded.txt"), "needle\n").unwrap();
        fs::write(cwd.join("visible.txt"), "needle\n").unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, &cwd, 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("visible.txt:1:needle"),
            "{result:?}"
        );
    }

    #[test]
    fn repository_excludes_apply_when_the_workspace_is_a_subdirectory() {
        let fixture = crate::workspace_tempdir().unwrap();
        let cwd = fixture.path().join("src");
        let info = fixture.path().join(".git/info");
        fs::create_dir_all(&info).unwrap();
        fs::create_dir(&cwd).unwrap();
        fs::write(info.join("exclude"), "/src/excluded.txt\n").unwrap();
        fs::write(cwd.join("excluded.txt"), "needle\n").unwrap();
        fs::write(cwd.join("visible.txt"), "needle\n").unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, &cwd, 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("visible.txt:1:needle"),
            "{result:?}"
        );
    }

    #[test]
    fn ancestor_ignore_rules_stop_at_the_repository_root() {
        let fixture = crate::workspace_tempdir().unwrap();
        let repo = fixture.path().join("repo");
        let parent = repo.join("src");
        let cwd = parent.join("workspace");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(fixture.path().join(".ignore"), "visible.txt\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            "\u{feff}/src/workspace/excluded.txt\nrestored.txt\n",
        )
        .unwrap();
        fs::write(
            parent.join(".ignore"),
            "more.txt\n!restored.txt\n!.hidden.txt\n",
        )
        .unwrap();
        for name in [
            "excluded.txt",
            "more.txt",
            "restored.txt",
            "visible.txt",
            ".hidden.txt",
        ] {
            fs::write(cwd.join(name), "needle\n").unwrap();
        }
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute_search(&request, &cwd, 4096, None, &|| false, |_| None);
        let output = result.output.unwrap();
        let mut lines = output.lines().collect::<Vec<_>>();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                ".hidden.txt:1:needle",
                "restored.txt:1:needle",
                "visible.txt:1:needle"
            ]
        );
    }

    #[test]
    fn outside_repository_excludes_require_an_association() {
        let fixture = crate::workspace_tempdir().unwrap();
        let root = fixture.path();
        let cwd = root.join("workspace");
        let common = root.join("repository.git");
        let worktree = common.join("worktrees/record");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir(common.join("info")).unwrap();
        fs::write(common.join("info/exclude"), "excluded.txt\n").unwrap();
        for name in ["excluded.txt", "visible.txt"] {
            fs::write(cwd.join(name), "needle\n").unwrap();
        }
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let search = || {
            execute_search(&request, &cwd, 4096, None, &|| false, |_| None)
                .output
                .unwrap()
        };
        fs::write(
            cwd.join(".git"),
            "gitdir: ../repository.git/worktrees/record\n",
        )
        .unwrap();
        fs::write(worktree.join("gitdir"), "../../../elsewhere/.git\n").unwrap();
        assert!(search().contains("excluded.txt"));
        fs::write(worktree.join("gitdir"), "../../../workspace/.git\n").unwrap();
        assert_eq!(search(), "visible.txt:1:needle");
        fs::write(cwd.join(".git"), "gitdir: ../repository.git\n").unwrap();
        fs::write(
            common.join("config"),
            "[other]\nworktree = ../workspace\n[core]\nworktree = ../elsewhere\n",
        )
        .unwrap();
        assert!(search().contains("excluded.txt"));
        fs::write(
            common.join("config"),
            "[core]\nworktree = \"../workspace\" # location\n",
        )
        .unwrap();
        assert_eq!(search(), "visible.txt:1:needle");
    }

    #[test]
    fn in_tree_gitignore_still_filters() {
        let fixture = crate::workspace_tempdir().unwrap();
        let cwd = fixture.path();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::write(cwd.join("src/.gitignore"), "ignored.txt\n").unwrap();
        fs::write(cwd.join("src/ignored.txt"), "needle\n").unwrap();
        fs::write(cwd.join("src/visible.txt"), "needle\n").unwrap();
        let request = grep_request(r#"{"pattern":"needle"}"#);
        let result = execute(&request, cwd, 4096);
        assert_eq!(
            result.output.as_deref(),
            Some("src/visible.txt:1:needle"),
            "{result:?}"
        );
    }

    #[test]
    fn a_pattern_starting_with_a_dash_is_searched_literally() {
        let cwd = temp_dir("grep-dash-pattern");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(
            cwd.join("lib.rs"),
            "fn load() -> Result<()> {\nfn other() {}\n",
        )
        .expect("write fixture");

        let result = execute(
            &grep_request(r#"{"pattern":"-> Result","path":"."}"#),
            &cwd,
            4096,
        );

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("fn load() -> Result<()> {"), "{output}");
    }

    #[test]
    fn option_shaped_patterns_are_searched_literally() {
        let workspace = crate::workspace_tempdir().unwrap();
        let cwd = workspace.path();
        fs::write(
            cwd.join("notes.txt"),
            "pass --files to list them\n-e option\n",
        )
        .unwrap();
        fs::write(cwd.join("other.txt"), "nothing here\n").unwrap();
        for (pattern, output) in [
            ("--files", "notes.txt:1:pass --files to list them"),
            ("-e", "notes.txt:2:-e option"),
        ] {
            let arguments = serde_json::json!({ "pattern": pattern });
            let result = execute(&grep_request(&arguments.to_string()), cwd, 4096);
            assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
            assert_eq!(result.output.as_deref(), Some(output));
        }
    }

    #[test]
    fn a_pattern_with_an_equals_sign_is_searched_literally() {
        let cwd = temp_dir("grep-equals-pattern");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "a=b\nab\n").expect("write fixture");

        let result = execute(&grep_request(r#"{"pattern":"a=b"}"#), &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("a=b"), "{output}");
    }

    #[test]
    fn an_empty_pattern_still_matches_every_line() {
        let cwd = temp_dir("grep-empty-pattern");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "first line\nsecond line\n").expect("write fixture");

        let result = execute(&grep_request(r#"{"pattern":""}"#), &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert_eq!(output.lines().count(), 2);
        assert!(output.contains("first line"), "{output}");
        assert!(output.contains("second line"), "{output}");
    }

    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        let root = temp_dir("grep-outside-path");
        let cwd = root.join("workspace");
        let outside = root.join("outside");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::create_dir_all(&outside).expect("create neighboring directory");
        fs::write(outside.join("notes.txt"), "needle outside the workspace\n")
            .expect("write neighboring fixture");

        for path in [Path::new(".."), outside.as_path()] {
            let arguments = serde_json::json!({"pattern": "needle", "path": path});
            let result = execute(&grep_request(&arguments.to_string()), &cwd, 4096);

            assert_eq!(result.status, ToolStatus::Failed, "{path:?}: {result:?}");
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("path escapes workspace")),
                "{path:?}: {result:?}",
            );
            assert!(
                !result
                    .output
                    .as_deref()
                    .unwrap_or_default()
                    .contains("needle outside the workspace"),
                "{path:?}: {result:?}",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leading_out_of_the_workspace_is_refused() {
        let cwd = temp_dir("grep-symlink-workspace");
        let outside = temp_dir("grep-symlink-neighbor");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::create_dir_all(&outside).expect("create neighboring directory");
        fs::write(outside.join("notes.txt"), "needle outside the workspace\n")
            .expect("write neighboring fixture");
        std::os::unix::fs::symlink(&outside, cwd.join("link")).expect("create symlink fixture");

        let request = grep_request(r#"{"pattern":"needle","path":"link"}"#);
        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Failed, "{result:?}");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("path escapes workspace")),
            "{result:?}",
        );
    }

    #[test]
    fn a_path_naming_the_workspace_itself_searches_everything() {
        let cwd = temp_dir("grep-workspace-path");
        fs::create_dir_all(cwd.join("src")).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "needle at the workspace root\n").expect("write fixture");

        for path in [".", "./", "src/.."] {
            let arguments = serde_json::json!({"pattern": "needle", "path": path});
            let result = execute(&grep_request(&arguments.to_string()), &cwd, 4096);

            assert_eq!(result.status, ToolStatus::Completed, "{path}: {result:?}");
            let output = result.output.as_deref().expect("grep output");
            assert!(output.contains("notes.txt:1:needle at the workspace root"));
        }
    }

    #[test]
    fn an_absolute_path_inside_the_workspace_is_searched() {
        let cwd = temp_dir("grep-absolute-path");
        let src = cwd.join("src");
        fs::create_dir_all(&src).expect("create temp workspace");
        fs::write(src.join("a.rs"), "needle in src\n").expect("write fixture");
        let arguments = serde_json::json!({"pattern": "needle", "path": src});

        let result = execute(&grep_request(&arguments.to_string()), &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert!(output.contains("a.rs:1:needle in src"), "{output}");
    }

    #[test]
    fn a_dot_slash_prefix_keeps_relative_output() {
        let workspace = crate::workspace_tempdir().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/notes.txt"), "needle\n").unwrap();
        let request = grep_request(r#"{"pattern":"needle","path":"./src"}"#);
        let result = execute(&request, workspace.path(), 4096);
        assert_eq!(result.output.as_deref(), Some("src/notes.txt:1:needle"));
    }

    #[test]
    fn a_path_named_dash_is_searched_as_a_file() {
        let cwd = temp_dir("grep-dash-path");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("-"), "needle in the file named dash\n").expect("write fixture");

        let request = grep_request(r#"{"pattern":"needle","path":"-"}"#);
        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert!(output.contains("needle in the file named dash"), "{output}");
    }

    #[test]
    fn search_matches_the_resolved_path() {
        let cwd = temp_dir("grep-fallback-dash-pattern");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(
            cwd.join("lib.rs"),
            "fn load() -> Result<()> {\nfn other() {}\n",
        )
        .expect("write fixture");
        let request = grep_request(r#"{"pattern":"-> Result","path":"."}"#);
        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        assert_eq!(
            result.output.as_deref(),
            Some("lib.rs:1:fn load() -> Result<()> {"),
        );
    }

    #[test]
    fn missing_search_path_completes_with_no_matches() {
        let cwd = temp_dir("grep-missing");
        fs::create_dir_all(cwd.join("tests")).expect("create temp workspace");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(r#"{"path":"tests/fixtures"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::NoMatches);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
        assert_eq!(result.error, None);
    }

    #[test]
    fn grep_defaults_to_first_250_results() {
        let cwd = temp_dir("grep-default-page");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let contents = (0..300)
            .map(|index| format!("needle {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(r#"{"pattern":"needle","path":"notes.txt"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 100_000);
        let output = result.output.as_deref().expect("grep output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 251);
        assert!(lines[0].contains("needle 000"));
        assert!(lines[249].contains("needle 249"));
        assert_eq!(
            lines[250],
            "[Showing first 250 results; use offset=250 to continue]"
        );
    }

    #[test]
    fn grep_respects_explicit_offset_and_head_limit() {
        let cwd = temp_dir("grep-offset-page");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let contents = (0..300)
            .map(|index| format!("needle {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(
                r#"{"pattern":"needle","path":"notes.txt","head_limit":10,"offset":250}"#
                    .to_string(),
            ),
        };

        let result = execute(&request, &cwd, 100_000);
        let output = result.output.as_deref().expect("grep output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 11);
        assert!(lines[0].contains("needle 250"));
        assert!(lines[9].contains("needle 259"));
        assert_eq!(
            lines[10],
            "[Showing results 251-260 of 300; use offset=260 to continue]"
        );
    }

    #[test]
    fn grep_zero_head_limit_uses_default_page() {
        let cwd = temp_dir("grep-zero-limit");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let contents = (0..300)
            .map(|index| format!("needle {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-zero-limit".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(
                r#"{"pattern":"needle","path":"notes.txt","head_limit":0}"#.to_string(),
            ),
        };

        let result = execute(&request, &cwd, 100_000);
        let lines = result
            .output
            .as_deref()
            .expect("grep output")
            .lines()
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), DEFAULT_GREP_HEAD_LIMIT + 1);
        assert!(lines[DEFAULT_GREP_HEAD_LIMIT - 1].contains("needle 249"));
        assert_eq!(
            lines[DEFAULT_GREP_HEAD_LIMIT],
            "[Showing first 250 results; use offset=250 to continue]"
        );
    }

    #[test]
    fn grep_head_limit_is_clamped_to_safety_ceiling() {
        assert_eq!(normalized_head_limit(Some(usize::MAX)), MAX_GREP_HEAD_LIMIT);
    }

    #[test]
    fn grep_paginates_the_complete_stream_before_retention() {
        let cwd = temp_dir("grep-complete-stream-page");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let padding = "x".repeat(4096);
        let contents = (0..500)
            .map(|index| format!("needle {index:03} {padding}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("notes.txt"), contents).expect("write fixture");
        let request = ToolRequest {
            id: "grep-middle-page".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(
                r#"{"pattern":"needle","path":"notes.txt","offset":240,"head_limit":5}"#
                    .to_string(),
            ),
        };

        let result = execute(&request, &cwd, 100_000);
        let lines = result
            .output
            .as_deref()
            .expect("grep output")
            .lines()
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains("needle 240"), "first line: {}", lines[0]);
        assert!(lines[4].contains("needle 244"), "last line: {}", lines[4]);
        assert_eq!(
            lines[5],
            "[Showing results 241-245 of 500; use offset=245 to continue]"
        );
    }

    #[test]
    fn grep_output_is_bounded_at_process_ingress() {
        let cwd = temp_dir("grep-bounded-ingress");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let line = format!("needle {}", "x".repeat(2 * 1024 * 1024));
        fs::write(cwd.join("notes.txt"), line).expect("write large fixture");
        let request = ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(
                r#"{"pattern":"needle","path":"notes.txt","head_limit":1}"#.to_string(),
            ),
        };

        let result = execute(&request, &cwd, 2 * 1024 * 1024);
        let output = result.output.as_deref().expect("grep output");

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.truncated);
        assert!(output.len() <= 1024 * 1024 + 128);
        assert!(output.contains("bytes of output omitted"));
    }

    #[test]
    fn search_reports_no_matches_with_the_builtin_contract() {
        let cwd = temp_dir("grep-fallback-no-matches");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "haystack\n").expect("write fixture");
        let request = ToolRequest {
            id: "grep-fallback-no-matches".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some("needle".to_string()),
            raw_arguments: Some(r#"{"pattern":"needle","path":"notes.txt"}"#.to_string()),
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::NoMatches);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
    }

    #[test]
    fn a_search_told_to_stop_is_cancelled() {
        let cwd = temp_dir("grep-cancel");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "needle\n").expect("write fixture");
        let request = ToolRequest {
            id: "grep-cancel".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(r#"{"pattern":"needle"}"#.to_string()),
        };

        let result = execute_or_cancel(&request, &cwd, 4096, || true);
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(result.error.as_deref(), Some("grep was cancelled"));
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn grep_keeps_a_cancellation_observed_once() {
        let workspace = crate::workspace_tempdir().unwrap();
        fs::write(workspace.path().join("notes.txt"), "needle\n").unwrap();
        let cancelled = Cell::new(true);
        let result = execute_or_cancel(
            &grep_request(r#"{"pattern":"needle"}"#),
            workspace.path(),
            4096,
            || cancelled.replace(false),
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
    }

    fn grep_request(arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "grep-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
