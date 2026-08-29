use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_GIT_STATUS_RETAINED_BYTES: usize = 8 * 1024;

pub fn status(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let mut command = Command::new("git");
    command
        .args(["status", "--short"])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::prepare_non_interactive_command(&mut command);
    let retained_bytes = max_bytes.clamp(
        MIN_GIT_STATUS_RETAINED_BYTES,
        crate::process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
    );
    let output = crate::process::spawn_user_trusted(command, "tool:git-status", cwd)
        .map(|(child, process_job, _receipt)| (child, process_job))
        .and_then(|(child, process_job)| {
            crate::process::wait_for_child_output_with_timeout_or_cancel_and_limit(
                child,
                process_job,
                GIT_STATUS_TIMEOUT,
                || false,
                retained_bytes,
            )
        });

    match output {
        Ok(output) if output.status.success() && !output.timed_out => {
            let stdout = output.stdout_text();
            let text = if stdout.trim().is_empty() {
                "(no changes)".to_string()
            } else {
                stdout
            };
            let (text, truncated) = truncate_output(text, max_bytes);
            let text =
                crate::process::preserve_ingress_omission_notice(text, output.stdout_omitted_bytes);
            ToolResult::completed(request, text, output.output_was_omitted() || truncated)
        }
        Ok(output) => {
            let stderr = output.stderr_text().trim().to_string();
            ToolResult::failed(
                request,
                if output.timed_out {
                    if stderr.is_empty() {
                        "git status timed out after 120s".to_string()
                    } else {
                        format!("git status timed out after 120s: {stderr}")
                    }
                } else {
                    stderr
                },
                output.status.code(),
            )
        }
        Err(error) => {
            ToolResult::failed(request, format!("failed to run git status: {error}"), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};

    use super::*;

    #[test]
    fn git_status_output_is_bounded_at_ingress() {
        let repo = tempfile::tempdir().expect("repo");
        let init = Command::new("git")
            .arg("init")
            .current_dir(repo.path())
            .status()
            .expect("git init");
        assert!(init.success());
        for index in 0..6_000 {
            let name = format!("untracked-{index:04}-{}.txt", "x".repeat(180));
            fs::write(repo.path().join(name), "x").expect("write untracked fixture");
        }
        let request = ToolRequest {
            id: "git-status-bounded".to_string(),
            name: ToolName::GitStatus,
            action: ActionKind::Read,
            target: None,
            raw_arguments: None,
        };

        let result = status(&request, repo.path(), 4 * 1024);

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.truncated);
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|output| output.contains("bytes of output omitted"))
        );
    }
}
