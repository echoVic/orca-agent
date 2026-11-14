use std::ops::ControlFlow;
use std::path::Path;

use serde::Deserialize;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind};

use crate::file_admission::{BoundedTextOutput, FileAdmissionError, stream_utf8_file};
use crate::read_document::document_kind;
use crate::resolve_workspace_path;

#[derive(Debug, Default, Deserialize)]
struct ReadFileArgs {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, cwd, max_bytes, || false)
}

pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = parse_args(request);
    let target = args.path.as_deref().or(request.target.as_deref());
    let path = match resolve_workspace_path(cwd, target) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
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
        Ok((output, truncated)) => {
            let kind = if truncated {
                ToolResultKind::Truncated
            } else {
                ToolResultKind::Success
            };
            ToolResult::completed_kind(request, output, truncated, kind)
        }
        // A document that is not text gets the tool that can read it.
        Err(FileAdmissionError::InvalidUtf8) if document_kind(&path).is_some() => {
            ToolResult::failed(
                request,
                format!(
                    "{} is {} file, not text; read it with read_document, which uses DeepSeek's OCR API",
                    path.display(),
                    document_kind(&path).unwrap_or("a document")
                ),
                None,
            )
        }
        Err(error) => ToolResult::failed(
            request,
            format!("failed to read {}: {error}", path.display()),
            None,
        ),
    }
}

/// `text` as read_file shows a file: the numbered lines `offset` and `limit`
/// select, or all of it, within `max_bytes`.
pub(crate) fn render_lines(
    text: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    max_bytes: usize,
) -> (String, bool) {
    if offset.is_some() || limit.is_some() {
        let mut collector = RangeCollector::new(offset.unwrap_or(1).max(1), limit, max_bytes);
        let _ = collector.push(text.as_bytes());
        collector.finish(true)
    } else {
        let mut output = BoundedTextOutput::new(max_bytes);
        output.append(text.as_bytes());
        output.finish()
    }
}

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

        let result = execute_or_cancel(&request, &cwd, 1024, || {
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

    #[test]
    fn read_file_points_binary_documents_at_read_document() {
        let cwd = temp_dir("read-file-binary-document");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("report.pdf"), b"%PDF-1.4\n\xff\xfe binary").expect("write document");
        let request = ToolRequest {
            id: "read-pdf".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("report.pdf".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Failed);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(
            error.ends_with(
                "report.pdf is a PDF file, not text; read it with read_document, which uses DeepSeek's OCR API"
            ),
            "{error}"
        );
    }

    #[test]
    fn read_file_still_reads_documents_that_are_text() {
        let cwd = temp_dir("read-file-text-document");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.doc"), "plain text notes\n").expect("write text document");
        let request = ToolRequest {
            id: "read-doc".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.doc".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 1024);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(result.output.as_deref(), Some("plain text notes\n"));
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
}
