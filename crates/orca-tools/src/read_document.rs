//! `read_document`: the text of a PDF, Word, PowerPoint or image file, as
//! DeepSeek's OCR API recognizes it.
//!
//! The file is uploaded, so this is a network tool: plan mode refuses it and
//! suggest mode asks first, as for the other DeepSeek APIs the tools call.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

use serde::Deserialize;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind};

use crate::deepseek_web::{self, DeepSeekWebAccess, DeepSeekWebError, OcrPage};
use crate::file_admission::open_regular_file;
use crate::read_file::render_lines;
use crate::resolve_workspace_path;

/// The largest file DeepSeek OCR accepts.
const MAX_OCR_BYTES: u64 = 100 * 1024 * 1024;
/// How many recognized documents the process keeps, so that reading one in
/// `offset`/`limit` chunks pays for its recognition once.
const OCR_CACHE_DOCUMENTS: usize = 8;

#[derive(Debug, Default, Deserialize)]
struct ReadDocumentArgs {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// What to call a file that `read_document` reads, by its extension; `None`
/// for any other file.
pub(crate) fn document_kind(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => Some("a PDF"),
        "doc" | "docx" => Some("a Word"),
        "ppt" | "pptx" => Some("a PowerPoint"),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" => Some("an image"),
        _ => None,
    }
}

/// Reads a document through DeepSeek OCR, or from the cache when this process
/// already recognized the same file. `offset` and `limit` count the lines of
/// the recognized text, as `read_file` counts a file's lines.
pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    deepseek: Option<&DeepSeekWebAccess>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ReadDocumentArgs>(raw).ok())
        .unwrap_or_default();
    let target = args.path.as_deref().or(request.target.as_deref());
    let path = match resolve_workspace_path(cwd, target) {
        Ok(path) => path,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let Some(kind) = document_kind(&path) else {
        return ToolResult::failed(
            request,
            format!(
                "{} is not a PDF, Word, PowerPoint or image file; read it with read_file",
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
    let (mut file, size) = match open_regular_file(&path) {
        Ok(opened) => opened,
        Err(error) => return failed(&error),
    };
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
    if size > MAX_OCR_BYTES {
        return too_large(size);
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = OcrKey {
        path: path.canonicalize().unwrap_or_else(|_| path.clone()),
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
            match runtime.block_on(deepseek_web::ocr(access, &file_name, bytes, &should_cancel)) {
                Ok(pages) => {
                    let pages = Arc::new(pages);
                    OCR_CACHE
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key, Arc::clone(&pages));
                    pages
                }
                Err(DeepSeekWebError::Cancelled) => {
                    return ToolResult::cancelled(request, "read_document was cancelled", None);
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
    let (output, truncated) = render_lines(
        &render_ocr(&file_name, &pages),
        args.offset,
        args.limit,
        max_bytes,
    );
    let kind = if truncated {
        ToolResultKind::Truncated
    } else {
        ToolResultKind::Success
    };
    ToolResult::completed_kind(request, output, truncated, kind)
}

/// The text `read_document` shows for a recognized document: a header, then
/// each page under its marker.
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

#[cfg(test)]
mod tests {
    use std::fs;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};
    use serde_json::json;

    use super::*;

    fn read_request(id: &str, raw_arguments: &str) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name: ToolName::ReadDocument,
            action: ActionKind::Network,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    /// A workspace holding one file, `name`, with `bytes`.
    fn workspace_with(name: &str, bytes: &[u8]) -> tempfile::TempDir {
        let cwd = tempfile::tempdir().expect("temp workspace");
        fs::write(cwd.path().join(name), bytes).expect("write document fixture");
        cwd
    }

    fn ocr_reply(pages: serde_json::Value) -> (u16, String) {
        (
            200,
            json!({"object": "ocr.result", "pages": pages}).to_string(),
        )
    }

    #[test]
    fn read_document_reads_a_pdf_through_ocr() {
        let cwd = workspace_with("report.pdf", b"%PDF-1.4 fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(json!([
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
            cwd.path(),
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
    fn read_document_is_cached() {
        let cwd = workspace_with("deck.pptx", b"PK fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(json!([
            {"index": 0, "text": "one\ntwo", "finish_reason": "stop"},
            {"index": 1, "text": "three", "finish_reason": "stop"},
        ]))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");
        let read = |id: &str, raw_arguments: &str| {
            execute_or_cancel(
                &read_request(id, raw_arguments),
                cwd.path(),
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
            path: PathBuf::from(name),
            size: 1,
            modified: None,
        };
        let pages = |text: &str| {
            Arc::new(vec![OcrPage {
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
    fn read_document_without_access_explains_what_it_needs() {
        let cwd = workspace_with("scan.png", b"\x89PNG fixture");

        let result = execute_or_cancel(
            &read_request("png", r#"{"path":"scan.png"}"#),
            cwd.path(),
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
    fn read_document_reports_a_missing_file_as_missing() {
        let cwd = tempfile::tempdir().expect("temp workspace");

        let result = execute_or_cancel(
            &read_request("missing", r#"{"path":"missing.pdf"}"#),
            cwd.path(),
            1024,
            None,
            || false,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.starts_with("failed to read "), "{error}");
        assert!(!error.contains("DeepSeek"), "{error}");
    }

    #[test]
    fn read_document_sends_text_files_to_read_file() {
        let cwd = workspace_with("notes.txt", b"plain text");
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("text", r#"{"path":"notes.txt"}"#),
            cwd.path(),
            1024,
            Some(&access),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result.error.as_deref().is_some_and(|error| error.ends_with(
                "notes.txt is not a PDF, Word, PowerPoint or image file; read it with read_file"
            )),
            "{:?}",
            result.error
        );
        deepseek.assert_untouched("DeepSeek");
    }

    #[test]
    fn read_document_refuses_files_over_100_mib() {
        let cwd = tempfile::tempdir().expect("temp workspace");
        // Sparse: only the length is set, so nothing is written.
        fs::File::create(cwd.path().join("big.pdf"))
            .and_then(|file| file.set_len(100 * 1024 * 1024 + 1))
            .expect("size the document fixture");
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("big", r#"{"path":"big.pdf"}"#),
            cwd.path(),
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
    fn read_document_can_be_cancelled_before_the_upload() {
        let cwd = workspace_with("a.pdf", b"%PDF");
        let deepseek = crate::test_http::untouched();
        let access = DeepSeekWebAccess::for_test(&deepseek.origin, "sk-test");

        let result = execute_or_cancel(
            &read_request("cancel", r#"{"path":"a.pdf"}"#),
            cwd.path(),
            1024,
            Some(&access),
            || true,
        );

        assert_eq!(result.status, ToolStatus::Cancelled, "{:?}", result.error);
        deepseek.assert_untouched("DeepSeek");
    }

    #[test]
    fn read_document_runs_inside_a_blocking_task() {
        // A tool that a runtime calls from `spawn_blocking` starts its own
        // runtime for the request.
        let cwd = workspace_with("a.jpg", b"\xff\xd8 fixture");
        let fixture = crate::test_http::serve(vec![ocr_reply(json!([
            {"index": 0, "text": "caption", "finish_reason": "stop"},
        ]))]);
        let access = DeepSeekWebAccess::for_test(&fixture.origin, "sk-test");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let workspace = cwd.path().to_path_buf();

        let result = runtime.block_on(async move {
            tokio::task::spawn_blocking(move || {
                execute_or_cancel(
                    &read_request("blocking", r#"{"path":"a.jpg"}"#),
                    &workspace,
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
