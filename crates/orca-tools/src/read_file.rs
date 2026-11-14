use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

use serde::Deserialize;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind};

use crate::deepseek_web::{self, DeepSeekWebAccess, DeepSeekWebError, OcrPage};
use crate::file_admission::{BoundedTextOutput, open_regular_file, stream_utf8_file};
use crate::resolve_workspace_path;

/// The largest file DeepSeek OCR accepts.
const MAX_OCR_BYTES: u64 = 100 * 1024 * 1024;
/// How many recognized documents the process keeps, so that reading one in
/// `offset`/`limit` chunks pays for its recognition once.
const OCR_CACHE_DOCUMENTS: usize = 8;

#[derive(Debug, Default, Deserialize)]
struct ReadFileArgs {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, cwd, max_bytes, None, || false)
}

/// Reads a text file, or the text DeepSeek OCR recognizes in a PDF, Word,
/// PowerPoint or image file when `deepseek` allows calling it.
pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = parse_args(request);
    let target = args.path.as_deref().or(request.target.as_deref());
    let path = match resolve_workspace_path(cwd, target) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if let Some(kind) = document_kind(&path) {
        return read_document(
            request,
            &path,
            kind,
            &args,
            max_bytes,
            deepseek,
            &should_cancel,
        );
    }
    let ranged = args.offset.is_some() || args.limit.is_some();
    let output = if ranged {
        let offset = args.offset.unwrap_or(1).max(1);
        stream_utf8_file(
            &path,
            RangeCollector::new(offset, args.limit, max_bytes),
            &should_cancel,
            |collector, chunk| collector.push(chunk),
        )
        .map(|outcome| outcome.value.finish(outcome.reached_eof))
    } else {
        stream_utf8_file(
            &path,
            BoundedTextOutput::new(max_bytes),
            &should_cancel,
            |output, chunk| {
                output.append(chunk);
                ControlFlow::Continue(())
            },
        )
        .map(|outcome| outcome.value.finish())
    };

    match output {
        Ok(output) => completed(request, output),
        Err(error) => ToolResult::failed(
            request,
            format!("failed to read {}: {error}", path.display()),
            None,
        ),
    }
}

fn completed(request: &ToolRequest, (output, truncated): (String, bool)) -> ToolResult {
    let kind = if truncated {
        ToolResultKind::Truncated
    } else {
        ToolResultKind::Success
    };
    ToolResult::completed_kind(request, output, truncated, kind)
}

/// What to call a file that is read through DeepSeek OCR, by its extension.
fn document_kind(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => Some("a PDF"),
        "doc" | "docx" => Some("a Word"),
        "ppt" | "pptx" => Some("a PowerPoint"),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" => Some("an image"),
        _ => None,
    }
}

/// Reads a document through DeepSeek OCR, or from the cache when this
/// process already recognized the same file.
fn read_document(
    request: &ToolRequest,
    path: &Path,
    kind: &str,
    args: &ReadFileArgs,
    max_bytes: usize,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: &dyn Fn() -> bool,
) -> ToolResult {
    let Some(access) = deepseek else {
        return ToolResult::failed(
            request,
            format!(
                "{} is {kind} file; Orca reads these through DeepSeek's OCR API, which needs the official DeepSeek endpoint (https://api.deepseek.com) with an API key and [tools] deepseek_web on.",
                path.display()
            ),
            None,
        );
    };
    let failed = |error: &dyn std::fmt::Display| {
        ToolResult::failed(
            request,
            format!("failed to read {}: {error}", path.display()),
            None,
        )
    };
    let too_large = |size: u64| {
        ToolResult::failed(
            request,
            format!(
                "{} has {size} bytes, and DeepSeek OCR reads files up to 100 MiB ({MAX_OCR_BYTES} bytes)",
                path.display()
            ),
            None,
        )
    };
    let (mut file, size) = match open_regular_file(path) {
        Ok(opened) => opened,
        Err(error) => return failed(&error),
    };
    if size > MAX_OCR_BYTES {
        return too_large(size);
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = OcrKey {
        path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        size,
        modified: file
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok()),
    };
    let cached = OCR_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key);
    let pages = match cached {
        Some(pages) => pages,
        None => {
            let mut bytes = Vec::new();
            if let Err(error) = (&mut file).take(MAX_OCR_BYTES + 1).read_to_end(&mut bytes) {
                return failed(&error);
            }
            drop(file);
            if bytes.len() as u64 > MAX_OCR_BYTES {
                return too_large(bytes.len() as u64);
            }
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to start the OCR runtime: {error}"),
                        None,
                    );
                }
            };
            match runtime.block_on(deepseek_web::ocr(access, &file_name, bytes, should_cancel)) {
                Ok(pages) => {
                    let pages = Arc::new(pages);
                    OCR_CACHE
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key, Arc::clone(&pages));
                    pages
                }
                Err(DeepSeekWebError::Cancelled) => {
                    return ToolResult::cancelled(request, "read_file was cancelled", None);
                }
                Err(DeepSeekWebError::Failed(message)) => {
                    return ToolResult::failed(
                        request,
                        format!("DeepSeek OCR could not read {}: {message}", path.display()),
                        None,
                    );
                }
            }
        }
    };
    completed(
        request,
        select_lines(&render_ocr(&file_name, &pages), args, max_bytes),
    )
}

/// The text read_file shows for a recognized document: a header, then each
/// page under its marker.
fn render_ocr(file_name: &str, pages: &[OcrPage]) -> String {
    let mut text = format!(
        "[Text recognized by DeepSeek OCR from {file_name}: {} page(s)]\n",
        pages.len()
    );
    for (position, page) in pages.iter().enumerate() {
        let number = position + 1;
        let _ = write!(text, "\n--- Page {number} ---\n");
        let page_text = page.text.trim_matches(['\r', '\n']);
        if page_text.trim().is_empty() {
            text.push_str("[no text on this page]\n");
        } else {
            text.push_str(page_text);
            text.push('\n');
        }
        if !page.complete {
            let _ = writeln!(
                text,
                "[page {number} was cut short: DeepSeek OCR stopped at its output limit]"
            );
        }
    }
    text
}

/// `text` as read_file shows a file: the numbered lines `offset`/`limit`
/// select, or all of it, within `max_bytes`.
fn select_lines(text: &str, args: &ReadFileArgs, max_bytes: usize) -> (String, bool) {
    if args.offset.is_some() || args.limit.is_some() {
        let offset = args.offset.unwrap_or(1).max(1);
        let mut collector = RangeCollector::new(offset, args.limit, max_bytes);
        let _ = collector.push(text.as_bytes());
        collector.finish(true)
    } else {
        let mut output = BoundedTextOutput::new(max_bytes);
        output.append(text.as_bytes());
        output.finish()
    }
}

/// A recognized file: where it is, and its size and modification time when
/// it was read, so that a changed file is recognized again.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OcrKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// The documents recognized most recently, newest first.
#[derive(Default)]
struct OcrCache {
    entries: VecDeque<(OcrKey, Arc<Vec<OcrPage>>)>,
}

impl OcrCache {
    fn get(&mut self, key: &OcrKey) -> Option<Arc<Vec<OcrPage>>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let pages = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(pages)
    }

    fn insert(&mut self, key: OcrKey, pages: Arc<Vec<OcrPage>>) {
        self.entries.retain(|(cached, _)| cached != &key);
        self.entries.push_front((key, pages));
        self.entries.truncate(OCR_CACHE_DOCUMENTS);
    }
}

static OCR_CACHE: LazyLock<Mutex<OcrCache>> = LazyLock::new(Mutex::default);

fn parse_args(request: &ToolRequest) -> ReadFileArgs {
    request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ReadFileArgs>(raw).ok())
        .unwrap_or_default()
}

struct RangeCollector {
    offset: usize,
    limit: Option<usize>,
    current_line: usize,
    total_lines: usize,
    selected_lines: usize,
    current_selected_started: bool,
    current_has_bytes: bool,
    pending_byte: Option<u8>,
    output: BoundedTextOutput,
}

impl RangeCollector {
    fn new(offset: usize, limit: Option<usize>, max_bytes: usize) -> Self {
        Self {
            offset,
            limit,
            current_line: 1,
            total_lines: 0,
            selected_lines: 0,
            current_selected_started: false,
            current_has_bytes: false,
            pending_byte: None,
            output: BoundedTextOutput::new(max_bytes),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> ControlFlow<()> {
        for &byte in bytes {
            if byte == b'\n' {
                if self.finish_line(true) {
                    return ControlFlow::Break(());
                }
                continue;
            }
            self.current_has_bytes = true;
            if let Some(previous) = self.pending_byte.replace(byte)
                && self.current_line_is_selected()
            {
                self.start_selected_line();
                self.output.append(&[previous]);
            }
        }
        ControlFlow::Continue(())
    }

    fn finish_line(&mut self, strip_carriage_return: bool) -> bool {
        let selected = self.current_line_is_selected();
        if let Some(last) = self.pending_byte.take()
            && selected
            && (!strip_carriage_return || last != b'\r')
        {
            self.start_selected_line();
            self.output.append(&[last]);
        }
        if selected {
            self.start_selected_line();
            self.selected_lines = self.selected_lines.saturating_add(1);
        }
        self.total_lines = self.current_line;
        let stop = self.limit.is_some_and(|limit| {
            (limit == 0 && self.total_lines >= self.offset)
                || (limit > 0 && self.selected_lines >= limit)
        });
        self.current_line = self.current_line.saturating_add(1);
        self.current_selected_started = false;
        self.current_has_bytes = false;
        stop
    }

    fn current_line_is_selected(&self) -> bool {
        self.current_line >= self.offset
            && self.limit.is_none_or(|limit| self.selected_lines < limit)
    }

    fn start_selected_line(&mut self) {
        if self.current_selected_started {
            return;
        }
        if self.selected_lines > 0 {
            self.output.append(b"\n");
        }
        self.output
            .append(format!("{}: ", self.current_line).as_bytes());
        self.current_selected_started = true;
    }

    fn finish(mut self, reached_eof: bool) -> (String, bool) {
        if reached_eof && self.current_has_bytes {
            self.finish_line(false);
        }
        if reached_eof && self.offset > self.total_lines {
            return (
                format!(
                    "[file has {} lines; requested offset {} is past end]",
                    self.total_lines, self.offset
                ),
                false,
            );
        }
        self.output.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[test]
    fn truncated_read_completes_with_truncated_kind() {
        let cwd = temp_dir("read-file-truncated");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "abcdef").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 3);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::Truncated);
        assert!(result.truncated);
        assert_eq!(result.output.as_deref(), Some("abc"));
    }

    #[test]
    fn read_file_respects_offset_and_limit() {
        let cwd = temp_dir("read-file-range");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "one\ntwo\nthree\nfour\n").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes.txt","offset":2,"limit":2}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("2: two\n3: three"));
        assert!(!result.truncated);
    }

    #[test]
    fn read_file_reports_short_file_when_offset_is_past_end() {
        let cwd = temp_dir("read-file-short");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "one\n").expect("write fixture");
        let request = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes.txt","offset":5,"limit":2}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("[file has 1 lines; requested offset 5 is past end]")
        );
    }

    #[test]
    fn read_file_rejects_non_regular_paths_before_reading() {
        let cwd = temp_dir("read-file-directory");
        fs::create_dir_all(cwd.join("nested")).expect("create directory fixture");
        let request = ToolRequest {
            id: "read-directory".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("nested".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not a regular file")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    fn ranged_read_bounds_a_selected_newline_free_line() {
        let cwd = temp_dir("read-file-huge-line");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(
            cwd.join("huge.txt"),
            format!("first\n{}\nlast", "x".repeat(2 * 1024 * 1024)),
        )
        .expect("write huge-line fixture");
        let request = ToolRequest {
            id: "read-huge-line".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("huge.txt".to_string()),
            raw_arguments: Some(r#"{"path":"huge.txt","offset":2,"limit":1}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::Truncated);
        assert!(result.truncated);
        assert!(result.output.as_deref().is_some_and(|output| {
            output.len() <= 1024 && output.starts_with("2: ") && output.contains("micro-compacted")
        }));
    }

    #[test]
    fn read_file_observes_cancellation_between_chunks() {
        let cwd = temp_dir("read-file-cancel");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("large.txt"), "x".repeat(128 * 1024))
            .expect("write cancellation fixture");
        let request = ToolRequest {
            id: "read-cancel".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("large.txt".to_string()),
            raw_arguments: None,
        };
        let polls = Cell::new(0);

        let result = execute_or_cancel(&request, &cwd, 1024, None, || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        });

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cancelled"))
        );
        assert_eq!(polls.get(), 2);
    }

    #[test]
    fn raw_read_rejects_invalid_utf8_beyond_the_retained_preview() {
        let cwd = temp_dir("read-file-invalid-utf8");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let mut bytes = vec![b'x'; 32 * 1024];
        bytes[24 * 1024] = 0xff;
        fs::write(cwd.join("invalid.txt"), bytes).expect("write invalid UTF-8 fixture");
        let request = ToolRequest {
            id: "read-invalid".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("invalid.txt".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("UTF-8"))
        );
    }

    #[test]
    fn ranged_read_preserves_crlf_and_unterminated_final_line_semantics() {
        let cwd = temp_dir("read-file-line-endings");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("lines.txt"), b"one\r\ntwo\r\nthree\r")
            .expect("write line-ending fixture");
        let request = ToolRequest {
            id: "read-line-endings".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("lines.txt".to_string()),
            raw_arguments: Some(r#"{"path":"lines.txt","offset":1}"#.to_string()),
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("1: one\n2: two\n3: three\r"));
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn read_request(id: &str, raw_arguments: &str) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    /// A workspace holding one file, `name`, with `bytes`.
    fn workspace_with(prefix: &str, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let cwd = temp_dir(prefix);
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join(name), bytes).expect("write document fixture");
        cwd
    }

    fn ocr_reply(pages: serde_json::Value) -> (u16, String) {
        (
            200,
            serde_json::json!({"object": "ocr.result", "pages": pages}).to_string(),
        )
    }

    #[test]
    fn read_file_reads_a_pdf_through_ocr() {
        let cwd = workspace_with("read-file-ocr", "report.pdf", b"%PDF-1.4 fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(serde_json::json!([
            {
                "index": 0,
                "text": "<｜ref｜>Revenue<｜/ref｜><｜box｜>[[52, 40, 948, 96]]<｜/box｜>\nup 12%\n",
                "finish_reason": "stop",
            },
            {"index": 1, "text": "Outlook", "finish_reason": "length"},
            {"index": 2, "text": "", "finish_reason": "stop"},
        ]))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("ocr", r#"{"path":"report.pdf"}"#),
            &cwd,
            64 * 1024,
            Some(&access),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(result.kind, ToolResultKind::Success);
        assert_eq!(
            result.output.as_deref(),
            Some(
                "[Text recognized by DeepSeek OCR from report.pdf: 3 page(s)]\n\
                 \n--- Page 1 ---\nRevenue\nup 12%\n\
                 \n--- Page 2 ---\nOutlook\n[page 2 was cut short: DeepSeek OCR stopped at its output limit]\n\
                 \n--- Page 3 ---\n[no text on this page]\n"
            )
        );
        assert_eq!(fixture.requests().len(), 1);
    }

    #[test]
    fn read_file_ocr_is_cached() {
        let cwd = workspace_with("read-file-ocr-cache", "deck.pptx", b"PK fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(serde_json::json!([
            {"index": 0, "text": "one\ntwo", "finish_reason": "stop"},
            {"index": 1, "text": "three", "finish_reason": "stop"},
        ]))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");
        let read = |id: &str, raw_arguments: &str| {
            execute_or_cancel(
                &read_request(id, raw_arguments),
                &cwd,
                64 * 1024,
                Some(&access),
                || false,
            )
        };

        let first = read("first", r#"{"path":"deck.pptx","offset":1,"limit":3}"#);
        let second = read("second", r#"{"path":"deck.pptx","offset":4,"limit":3}"#);

        assert_eq!(
            first.output.as_deref(),
            Some(
                "1: [Text recognized by DeepSeek OCR from deck.pptx: 2 page(s)]\n2: \n3: --- Page 1 ---"
            ),
            "{:?}",
            first.error
        );
        assert_eq!(
            second.output.as_deref(),
            Some("4: one\n5: two\n6: "),
            "the second read must come from the cache: {:?}",
            second.error
        );
        assert_eq!(fixture.requests().len(), 1);
    }

    #[test]
    fn ocr_cache_keeps_the_most_recent_documents() {
        let key = |name: &str| OcrKey {
            path: std::path::PathBuf::from(name),
            size: 1,
            modified: None,
        };
        let pages = |text: &str| {
            std::sync::Arc::new(vec![OcrPage {
                text: text.to_string(),
                complete: true,
            }])
        };
        let mut cache = OcrCache::default();
        for number in 0..OCR_CACHE_DOCUMENTS {
            cache.insert(key(&number.to_string()), pages("old"));
        }

        assert!(cache.get(&key("0")).is_some(), "a hit counts as a use");
        cache.insert(key("new"), pages("new"));

        assert!(cache.get(&key("0")).is_some());
        assert!(
            cache.get(&key("1")).is_none(),
            "the least recent goes first"
        );
        assert!(cache.get(&key("new")).is_some());
        assert_eq!(
            cache.get(&OcrKey {
                size: 2,
                ..key("new")
            }),
            None,
            "a changed file is another document"
        );
    }

    #[test]
    fn read_file_without_access_explains_documents() {
        let cwd = workspace_with("read-file-ocr-none", "scan.png", b"\x89PNG fixture");

        let result = execute_or_cancel(
            &read_request("png", r#"{"path":"scan.png"}"#),
            &cwd,
            1024,
            None,
            || false,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(
            error.ends_with(
                "scan.png is an image file; Orca reads these through DeepSeek's OCR API, which needs the official DeepSeek endpoint (https://api.deepseek.com) with an API key and [tools] deepseek_web on."
            ),
            "{error}"
        );
    }

    #[test]
    fn read_file_refuses_documents_over_100_mib() {
        let cwd = temp_dir("read-file-ocr-large");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        // Sparse: only the length is set, so nothing is written.
        fs::File::create(cwd.join("big.pdf"))
            .and_then(|file| file.set_len(100 * 1024 * 1024 + 1))
            .expect("size the document fixture");
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("big", r#"{"path":"big.pdf"}"#),
            &cwd,
            1024,
            Some(&access),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("DeepSeek OCR reads files up to 100 MiB")),
            "{:?}",
            result.error
        );
        deepseek.assert_untouched("DeepSeek");
    }

    #[test]
    fn read_file_ocr_can_be_cancelled_before_the_upload() {
        let cwd = workspace_with("read-file-ocr-cancel", "a.pdf", b"%PDF");
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("cancel", r#"{"path":"a.pdf"}"#),
            &cwd,
            1024,
            Some(&access),
            || true,
        );

        assert_eq!(result.status, ToolStatus::Cancelled, "{:?}", result.error);
        deepseek.assert_untouched("DeepSeek");
    }

    #[test]
    fn read_file_ocr_runs_inside_a_blocking_task() {
        // The runtime's read-only batch calls tools from `spawn_blocking`.
        let cwd = workspace_with("read-file-ocr-blocking", "a.jpg", b"\xff\xd8 fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(serde_json::json!([
            {"index": 0, "text": "caption", "finish_reason": "stop"},
        ]))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");

        let result = runtime.block_on(async move {
            tokio::task::spawn_blocking(move || {
                execute_or_cancel(
                    &read_request("blocking", r#"{"path":"a.jpg"}"#),
                    &cwd,
                    1024,
                    Some(&access),
                    || false,
                )
            })
            .await
            .expect("blocking read")
        });

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|output| output.ends_with("--- Page 1 ---\ncaption\n")),
            "{:?}",
            result.output
        );
        fixture.requests();
    }
}
