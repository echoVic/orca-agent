use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ignore::WalkBuilder;
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use regex::Regex;
use serde::Deserialize;

const DEFAULT_GREP_HEAD_LIMIT: usize = 250;
const MAX_GREP_HEAD_LIMIT: usize = 1_000;
const MIN_GREP_LINE_BYTES: usize = 8 * 1024;
const MAX_GREP_LINE_BYTES: usize = 1024 * 1024;

#[derive(Default, Deserialize)]
struct GrepArgs {
    pattern: Option<String>,
    path: Option<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let args = parse_args(request);
    let pattern = args.pattern.as_deref().or(request
        .target
        .as_deref()
        .filter(|target| !target.is_empty()));
    let Some(pattern) = pattern else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };

    let search_path = args.path.unwrap_or_else(|| ".".to_string());

    if !cwd.join(&search_path).exists() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let mut command = Command::new("rg");
    command
        .args(["--line-number", "--no-heading", pattern, &search_path])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::prepare_non_interactive_command(&mut command);
    let offset = args.offset.unwrap_or(0);
    let limit = normalized_head_limit(args.head_limit);
    let collector = GrepPageCollector::new(offset, limit, max_bytes.max(1));
    let max_line_bytes = max_bytes.clamp(MIN_GREP_LINE_BYTES, MAX_GREP_LINE_BYTES);
    let output = crate::process::spawn_user_trusted(command, "tool:grep", cwd)
        .map(|(child, process_job, _receipt)| (child, process_job))
        .and_then(|(child, process_job)| {
            crate::process::wait_for_child_stdout_lines_with_timeout(
                child,
                process_job,
                Duration::from_secs(120),
                max_line_bytes,
                collector,
                |collector, line| {
                    collector.push(line);
                    Ok(())
                },
            )
        });

    match output {
        Ok(output) if output.status.success() && !output.timed_out => {
            let ingress_truncated = output.output_was_omitted();
            let (stdout, page_truncated) = output.value.render();
            let (stdout, truncated) = truncate_output(stdout, max_bytes);
            let stdout = crate::process::preserve_ingress_omission_notice(
                stdout,
                output.stdout_omitted_bytes,
            );
            ToolResult::completed(
                request,
                stdout,
                ingress_truncated || page_truncated || truncated,
            )
        }
        Ok(output) if output.timed_out => {
            let stderr = output.stderr_text().trim().to_string();
            ToolResult::failed(
                request,
                if stderr.is_empty() {
                    "rg timed out after 120s".to_string()
                } else {
                    format!("rg timed out after 120s: {stderr}")
                },
                output.status.code(),
            )
        }
        Ok(output) if output.status.code() == Some(1) => ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        ),
        Ok(output) => {
            let stderr = output.stderr_text().trim().to_string();
            ToolResult::failed(request, stderr, output.status.code())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            execute_in_process(request, cwd, pattern, &search_path, max_bytes)
        }
        Err(error) => ToolResult::failed(request, format!("failed to run rg: {error}"), None),
    }
}

fn execute_in_process(
    request: &ToolRequest,
    cwd: &Path,
    pattern: &str,
    search_path: &str,
    max_bytes: usize,
) -> ToolResult {
    let regex = match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolResult::failed(request, error.to_string(), None),
    };

    let offset = parse_args(request).offset.unwrap_or(0);
    let limit = normalized_head_limit(parse_args(request).head_limit);
    let mut collector = GrepPageCollector::new(offset, limit, max_bytes.max(1));
    let max_line_bytes = max_bytes.clamp(MIN_GREP_LINE_BYTES, MAX_GREP_LINE_BYTES);
    let search_root = cwd.join(search_path);
    let mut walker = WalkBuilder::new(search_root);
    walker
        .hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    for entry in walker.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        let path = entry.path();
        let Ok(file) = File::open(path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let display_path = path
            .strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let mut line_number = 1;
        while let Some(line) = read_bounded_line(&mut reader, max_line_bytes) {
            if regex.is_match(&String::from_utf8_lossy(&line.bytes)) {
                let prefix = format!("{display_path}:{line_number}:");
                let mut output_line = Vec::with_capacity(prefix.len() + line.bytes.len());
                output_line.extend_from_slice(prefix.as_bytes());
                output_line.extend_from_slice(&line.bytes);
                collector.push(crate::process::BoundedLine {
                    bytes: &output_line,
                    observed_bytes: output_line.len().saturating_add(line.omitted_bytes),
                    omitted_bytes: line.omitted_bytes,
                });
            }
            line_number += 1;
        }
    }

    let no_matches = collector.total_lines == 0;
    let (output, page_truncated) = collector.render();
    if no_matches {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }
    let (output, truncated) = truncate_output(output, max_bytes);
    ToolResult::completed(request, output, page_truncated || truncated)
}

struct BoundedInputLine {
    bytes: Vec<u8>,
    omitted_bytes: usize,
}

fn read_bounded_line(reader: &mut impl BufRead, max_bytes: usize) -> Option<BoundedInputLine> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut omitted_bytes: usize = 0;
    let mut saw_bytes = false;

    loop {
        let buffer = reader.fill_buf().ok()?;
        if buffer.is_empty() {
            break;
        }
        saw_bytes = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(buffer.len());
        let remaining = max_bytes.saturating_sub(bytes.len());
        let retained_len = content_len.min(remaining);
        bytes.extend_from_slice(&buffer[..retained_len]);
        omitted_bytes = omitted_bytes.saturating_add(content_len - retained_len);
        let consumed = newline.map_or(content_len, |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if !saw_bytes {
        return None;
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Some(BoundedInputLine {
        bytes,
        omitted_bytes,
    })
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
    lines: Vec<String>,
    selection_truncated: bool,
}

impl GrepPageCollector {
    fn new(offset: usize, limit: usize, retained_budget: usize) -> Self {
        Self {
            offset,
            limit,
            total_lines: 0,
            retained_bytes: 0,
            retained_budget,
            lines: Vec::with_capacity(limit.min(DEFAULT_GREP_HEAD_LIMIT)),
            selection_truncated: false,
        }
    }

    fn push(&mut self, line: crate::process::BoundedLine<'_>) {
        let index = self.total_lines;
        self.total_lines = self.total_lines.saturating_add(1);
        if index < self.offset || self.lines.len() >= self.limit {
            return;
        }

        let separator_bytes = usize::from(!self.lines.is_empty());
        let remaining = self
            .retained_budget
            .saturating_sub(self.retained_bytes.saturating_add(separator_bytes));
        if remaining == 0 {
            self.selection_truncated = true;
            return;
        }

        let mut text = String::from_utf8_lossy(line.bytes).to_string();
        if line.omitted_bytes > 0 {
            text = crate::process::preserve_ingress_omission_notice(text, line.omitted_bytes);
        }
        let (text, truncated) = truncate_output(text, remaining);
        self.selection_truncated |= truncated || line.omitted_bytes > 0;
        self.retained_bytes = self
            .retained_bytes
            .saturating_add(separator_bytes)
            .saturating_add(text.len());
        self.lines.push(text);
    }

    fn render(self) -> (String, bool) {
        let next_offset = (self.total_lines.saturating_sub(self.offset) > self.limit)
            .then_some(self.offset.saturating_add(self.limit));

        let mut output = self.lines.join("\n");
        if let Some(next_offset) = next_offset {
            let notice = if self.offset == 0 {
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
            };
            if output.is_empty() {
                output = notice;
            } else {
                output.push('\n');
                output.push_str(&notice);
            }
        }
        (output, self.selection_truncated)
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
    fn missing_search_path_completes_with_no_matches() {
        let cwd = temp_dir("grep-missing");
        fs::create_dir_all(&cwd).expect("create temp workspace");
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
    fn in_process_fallback_reports_no_matches_with_the_builtin_contract() {
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

        let result = execute_in_process(&request, &cwd, "needle", "notes.txt", 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::NoMatches);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
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
