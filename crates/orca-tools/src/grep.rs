use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ignore::WalkBuilder;
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use regex::Regex;
use serde::Deserialize;

use crate::spill::ReadTarget;

const DEFAULT_GREP_HEAD_LIMIT: usize = 250;
const MAX_GREP_HEAD_LIMIT: usize = 1_000;
const MIN_GREP_LINE_BYTES: usize = 8 * 1024;
const MAX_GREP_LINE_BYTES: usize = 1024 * 1024;
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
    let args = parse_args(request);
    let pattern = args.pattern.as_deref().or(request
        .target
        .as_deref()
        .filter(|target| !target.is_empty()));
    let Some(pattern) = pattern else {
        return ToolResult::failed(request, "grep pattern is required", None);
    };

    let requested_path = args.path.as_deref().unwrap_or(".");
    let (search_path, spill) =
        match crate::spill::resolve_read_path(cwd, Some(requested_path), spill_dir) {
            Ok(ReadTarget::Workspace(path)) => (path, None),
            Ok(ReadTarget::Spill(path, file)) => (path, Some(file)),
            Err(message) => return ToolResult::failed(request, message, None),
        };

    if spill.is_none() && !search_path.exists() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let rg_path = if spill.is_some() {
        Path::new("-")
    } else if Path::new(requested_path).is_absolute() {
        search_path.as_path()
    } else {
        let relative = search_path
            .strip_prefix(cwd)
            .expect("resolved path is inside the workspace");
        if relative.as_os_str().is_empty() {
            Path::new(".")
        } else if relative.as_os_str() == "-" {
            // Ripgrep reads stdin for a bare "-", so spell the file path explicitly.
            Path::new("./-")
        } else {
            relative
        }
    };

    let mut command = Command::new("rg");
    command
        .args(rg_args(pattern, rg_path))
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::prepare_non_interactive_command(&mut command);
    if let Some(file) = spill.as_ref() {
        let input = match file.try_clone() {
            Ok(input) => input,
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!("could not read saved tool result: {error}"),
                    None,
                );
            }
        };
        // Pass the already verified file as stdin, so the helper never
        // resolves its pathname again.
        command.stdin(Stdio::from(input));
    }
    let offset = args.offset.unwrap_or(0);
    let limit = normalized_head_limit(args.head_limit);
    let collector = GrepPageCollector::new(offset, limit, max_bytes.max(1));
    let max_line_bytes = max_bytes.clamp(MIN_GREP_LINE_BYTES, MAX_GREP_LINE_BYTES);
    let output = crate::process::spawn_user_trusted(command, "tool:grep", cwd)
        .map(|(child, process_job, _receipt)| (child, process_job))
        .and_then(|(child, process_job)| {
            crate::process::wait_for_child_stdout_lines_with_timeout_or_cancel(
                child,
                process_job,
                Duration::from_secs(120),
                max_line_bytes,
                crate::process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
                collector,
                &should_cancel,
                |collector, line| {
                    collector.push(line);
                    Ok(())
                },
            )
        });

    match output {
        Ok(output) if output.cancelled => ToolResult::cancelled(request, GREP_CANCELLED, None),
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
            if let Some(file) = spill {
                let regex = match Regex::new(pattern) {
                    Ok(regex) => regex,
                    Err(error) => return ToolResult::failed(request, error.to_string(), None),
                };
                let mut collector = GrepPageCollector::new(offset, limit, max_bytes.max(1));
                if !search_file(
                    file,
                    &search_path,
                    &regex,
                    MAX_GREP_LINE_BYTES,
                    &mut collector,
                    &should_cancel,
                ) {
                    return ToolResult::cancelled(request, GREP_CANCELLED, None);
                }
                render_search(request, collector, max_bytes)
            } else {
                execute_in_process(
                    request,
                    cwd,
                    pattern,
                    &search_path,
                    max_bytes,
                    &should_cancel,
                )
            }
        }
        Err(error) => ToolResult::failed(request, format!("failed to run rg: {error}"), None),
    }
}

fn rg_args(pattern: &str, search_path: &Path) -> Vec<OsString> {
    vec![
        "--no-config".into(),
        "--line-number".into(),
        "--no-heading".into(),
        format!("--regexp={pattern}").into(),
        "--".into(),
        search_path.as_os_str().to_owned(),
    ]
}

fn execute_in_process(
    request: &ToolRequest,
    cwd: &Path,
    pattern: &str,
    search_path: &Path,
    max_bytes: usize,
    should_cancel: &dyn Fn() -> bool,
) -> ToolResult {
    let regex = match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return ToolResult::failed(request, error.to_string(), None),
    };

    let offset = parse_args(request).offset.unwrap_or(0);
    let limit = normalized_head_limit(parse_args(request).head_limit);
    let mut collector = GrepPageCollector::new(offset, limit, max_bytes.max(1));
    let mut walker = WalkBuilder::new(search_path);
    walker
        .hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    for entry in walker.build() {
        if should_cancel() {
            return ToolResult::cancelled(request, GREP_CANCELLED, None);
        }
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
        if !search_file(
            file,
            path.strip_prefix(cwd).unwrap_or(path),
            &regex,
            max_bytes.clamp(MIN_GREP_LINE_BYTES, MAX_GREP_LINE_BYTES),
            &mut collector,
            should_cancel,
        ) {
            return ToolResult::cancelled(request, GREP_CANCELLED, None);
        }
    }
    render_search(request, collector, max_bytes)
}

fn search_file(
    file: File,
    path: &Path,
    regex: &Regex,
    max_line_bytes: usize,
    collector: &mut GrepPageCollector,
    should_cancel: &dyn Fn() -> bool,
) -> bool {
    let mut reader = BufReader::new(file);
    let path = path.to_string_lossy().replace('\\', "/");
    let mut line_number = 1;
    while let Some(line) = read_bounded_line(&mut reader, max_line_bytes) {
        if should_cancel() {
            return false;
        }
        if regex.is_match(&String::from_utf8_lossy(&line.bytes)) {
            let prefix = format!("{path}:{line_number}:");
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
    true
}

fn render_search(
    request: &ToolRequest,
    collector: GrepPageCollector,
    max_bytes: usize,
) -> ToolResult {
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
    fn rg_args_pass_the_pattern_and_path_as_values() {
        assert_eq!(
            rg_args("-> Result", Path::new("src")),
            [
                "--no-config",
                "--line-number",
                "--no-heading",
                "--regexp=-> Result",
                "--",
                "src",
            ]
            .map(std::ffi::OsString::from),
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
    fn a_pattern_naming_an_rg_option_is_searched_literally() {
        let cwd = temp_dir("grep-option-name");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(cwd.join("notes.txt"), "pass --files to list them\n").expect("write fixture");
        fs::write(cwd.join("other.txt"), "nothing here\n").expect("write fixture");

        let result = execute(&grep_request(r#"{"pattern":"--files"}"#), &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        let output = result.output.as_deref().expect("grep output");
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("pass --files to list them"), "{output}");
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
    fn the_fallback_searches_the_resolved_path_literally() {
        let cwd = temp_dir("grep-fallback-dash-pattern");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::write(
            cwd.join("lib.rs"),
            "fn load() -> Result<()> {\nfn other() {}\n",
        )
        .expect("write fixture");
        let request = grep_request(r#"{"pattern":"-> Result","path":"."}"#);
        let search_path =
            crate::resolve_workspace_path(&cwd, Some(".")).expect("resolve workspace");

        let result = execute_in_process(&request, &cwd, "-> Result", &search_path, 4096, &|| false);

        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        assert_eq!(
            result.output.as_deref(),
            Some("lib.rs:1:fn load() -> Result<()> {"),
        );
    }

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

        let result = execute_in_process(
            &request,
            &cwd,
            "needle",
            &cwd.join("notes.txt"),
            4096,
            &|| false,
        );

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

        // With `rg`, and without it.
        let results = [
            execute_or_cancel(&request, &cwd, 4096, || true),
            execute_in_process(&request, &cwd, "needle", &cwd, 4096, &|| true),
        ];

        for result in results {
            assert_eq!(result.status, ToolStatus::Cancelled);
            assert_eq!(result.error.as_deref(), Some("grep was cancelled"));
        }
        let _ = fs::remove_dir_all(&cwd);
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
        std::env::temp_dir().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
