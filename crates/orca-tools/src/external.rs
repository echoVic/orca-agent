use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};
use orca_platform::process::ProcessJob;

use crate::process;

const MAX_EXTERNAL_TOOL_ENV_ARGS_BYTES: usize = 64 * 1024;

// Security: only loads from ORCA_HOME/tools/ (user-controlled), never from
// project-level directories, to prevent repo poisoning attacks.
pub fn default_tools_dir() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join("tools"))
}

pub fn load_default_external_tools() -> Vec<ExternalToolConfig> {
    default_tools_dir()
        .as_deref()
        .map(load_external_tools_dir)
        .unwrap_or_default()
}

pub fn load_external_tools_dir(dir: &Path) -> Vec<ExternalToolConfig> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            eprintln!(
                "orca: warning: failed to read external tools directory '{}': {error}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut tools = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .filter_map(|path| {
            let content = fs::read_to_string(&path).ok()?;
            match toml::from_str::<ExternalToolConfig>(&content) {
                Ok(tool) if is_valid_tool_name(&tool.name) => Some(tool),
                Ok(tool) => {
                    eprintln!(
                        "orca: warning: ignoring external tool with invalid name '{}'",
                        tool.name
                    );
                    None
                }
                Err(error) => {
                    eprintln!(
                        "orca: warning: failed to parse external tool '{}': {error}",
                        path.display()
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

pub fn execute_external_tool(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    max_output_bytes: usize,
) -> ToolResult {
    execute_external_tool_with_policy(
        config,
        request,
        cwd,
        ToolOutputTruncation::bytes(max_output_bytes),
        Duration::from_secs(120),
    )
}

pub fn execute_external_tool_with_policy(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
) -> ToolResult {
    execute_external_tool_with_policy_or_cancel(
        config,
        request,
        cwd,
        output_truncation,
        shell_timeout,
        || false,
    )
}

pub fn execute_external_tool_with_policy_or_cancel(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let args = request.raw_arguments.as_deref().unwrap_or("{}");
    let shell =
        match orca_platform::shell::ShellResolver::for_current_host().resolve_from_environment() {
            Ok(shell) => shell,
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!(
                        "external tool '{}' could not resolve the host shell: {error}",
                        config.name
                    ),
                    None,
                );
            }
        };
    let mut command = process::shell_command(&shell, &config.command);
    command
        .current_dir(cwd)
        .env("ORCA_TOOL_NAME", &config.name)
        .env(
            "ORCA_TOOL_TARGET",
            request.target.as_deref().unwrap_or_default(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if args.len() <= MAX_EXTERNAL_TOOL_ENV_ARGS_BYTES {
        command.env("ORCA_TOOL_ARGS", args);
    } else {
        command.env_remove("ORCA_TOOL_ARGS");
    }
    process::prepare_non_interactive_command(&mut command);
    command.stdin(Stdio::piped());

    let (mut child, process_job) = match ProcessJob::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("external tool '{}' failed to start: {error}", config.name),
                None,
            );
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(args.as_bytes()) {
            process::terminate_child_tree(&mut child, &process_job);
            let exit_code = child.wait().ok().and_then(|status| status.code());
            return ToolResult::failed(
                request,
                format!(
                    "external tool '{}' failed to receive input: {error}",
                    config.name
                ),
                exit_code,
            );
        }
    }

    let output = match process::wait_for_child_output_with_timeout_or_cancel(
        child,
        process_job,
        shell_timeout,
        &should_cancel,
    ) {
        Ok(output) => output,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("external tool '{}' failed: {error}", config.name),
                None,
            );
        }
    };

    let ingress_truncated = output.output_was_omitted();
    let stdout = output.stdout_text();
    let stderr = output.stderr_text().trim().to_string();
    if output.termination == process::CommandTermination::Cancelled {
        let stdout = stdout.trim();
        let detail = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout.to_string(),
            (true, false) => stderr,
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        let message = if detail.is_empty() {
            format!("external tool '{}' cancelled", config.name)
        } else {
            format!("external tool '{}' cancelled: {detail}", config.name)
        };
        let (message, truncated) = truncate_output_with_policy(message, output_truncation);
        let message = process::preserve_ingress_omission_notice(
            message,
            output
                .stdout_omitted_bytes
                .saturating_add(output.stderr_omitted_bytes),
        );
        let mut result = ToolResult::cancelled(request, message, output.status.code());
        result.set_truncated(ingress_truncated || truncated);
        return result;
    }
    if output.status.success() && !output.timed_out {
        let (result_output, truncated) = truncate_output_with_policy(stdout, output_truncation);
        let result_output =
            process::preserve_ingress_omission_notice(result_output, output.stdout_omitted_bytes);
        return ToolResult::completed(request, result_output, ingress_truncated || truncated);
    }

    let stdout = stdout.trim();
    let detail = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    };
    let message = if output.timed_out {
        if detail.is_empty() {
            format!(
                "external tool '{}' timed out after {}s",
                config.name,
                shell_timeout.as_secs()
            )
        } else {
            format!(
                "external tool '{}' timed out after {}s: {detail}",
                config.name,
                shell_timeout.as_secs()
            )
        }
    } else if detail.is_empty() {
        format!(
            "external tool '{}' exited with {}",
            config.name, output.status
        )
    } else {
        format!(
            "external tool '{}' exited with {}: {detail}",
            config.name, output.status
        )
    };
    let (message, truncated) = truncate_output_with_policy(message, output_truncation);
    let message = process::preserve_ingress_omission_notice(
        message,
        output
            .stdout_omitted_bytes
            .saturating_add(output.stderr_omitted_bytes),
    );
    let mut result = ToolResult::failed(request, message, output.status.code());
    result.set_truncated(ingress_truncated || truncated);
    result
}

fn is_valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::time::Instant;

    fn platform_script(unix: impl Into<String>, windows: impl Into<String>) -> String {
        if cfg!(windows) {
            windows.into()
        } else {
            unix.into()
        }
    }

    fn platform_delay(unix_ms: u64, windows_ms: u64) -> Duration {
        Duration::from_millis(if cfg!(windows) { windows_ms } else { unix_ms })
    }

    #[test]
    fn external_tool_timeout_kills_descendant_processes() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = ExternalToolConfig {
            name: "slow_tool".to_string(),
            description: "slow tool".to_string(),
            action_kind: ActionKind::Shell,
            command: platform_script(
                "printf stdout-before; printf stderr-before >&2; sleep 5; sleep 5; printf after",
                "[Console]::Out.Write('stdout-before'); [Console]::Out.Flush(); [Console]::Error.Write('stderr-before'); [Console]::Error.Flush(); Start-Sleep -Seconds 5; Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
            ),
            schema: serde_json::json!({}),
        };
        let request = ToolRequest {
            id: "external-1".to_string(),
            name: ToolName::External("slow_tool".to_string()),
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let start = Instant::now();

        let timeout = platform_delay(200, 1_500);
        let result = execute_external_tool_with_policy(
            &config,
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            timeout,
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "external tool should not wait for descendant processes"
        );
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("stdout-before"), "missing stdout: {error}");
        assert!(error.contains("stderr-before"), "missing stderr: {error}");
        assert_eq!(result.status, ToolStatus::Failed);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(
            error.contains(&format!(
                "external tool 'slow_tool' timed out after {}s",
                timeout.as_secs()
            )),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            !error.contains("beforeafter"),
            "timeout should kill descendants before the trailing command runs: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_tool_timeout_preserves_observed_exit_code() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = ExternalToolConfig {
            name: "slow_tool".to_string(),
            description: "slow tool".to_string(),
            action_kind: ActionKind::Shell,
            command: "trap 'exit 42' TERM; printf before; while :; do :; done".to_string(),
            schema: serde_json::json!({}),
        };
        let request = ToolRequest {
            id: "external-timeout-exit".to_string(),
            name: ToolName::External("slow_tool".to_string()),
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        let result = execute_external_tool_with_policy(
            &config,
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            // Leave enough startup budget for a loaded serial workspace run to
            // install the TERM trap. This test checks exit-code preservation.
            Duration::from_secs(3),
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.exit_code, Some(42));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("before"))
        );
    }

    #[test]
    fn external_tool_failure_combines_and_truncates_diagnostics() {
        let dir = tempfile::TempDir::new().unwrap();
        let stdout = format!("stdout-head:{}", "o".repeat(160));
        let stderr = format!("{}:stderr-tail", "e".repeat(160));
        let config = ExternalToolConfig {
            name: "failing_tool".to_string(),
            description: "failing tool".to_string(),
            action_kind: ActionKind::Shell,
            command: platform_script(
                format!("printf {stdout:?}; printf {stderr:?} >&2; exit 7"),
                format!(
                    "[Console]::Out.Write('{}'); [Console]::Error.Write('{}'); exit 7",
                    stdout.replace('\'', "''"),
                    stderr.replace('\'', "''"),
                ),
            ),
            schema: serde_json::json!({}),
        };
        let request = ToolRequest {
            id: "external-failure".to_string(),
            name: ToolName::External("failing_tool".to_string()),
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        let result = execute_external_tool_with_policy(
            &config,
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(180),
            Duration::from_secs(5),
        );

        let error = result.error.as_deref().unwrap_or_default();
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.exit_code, Some(7));
        assert!(result.truncated);
        assert!(
            error.len() <= 180,
            "untruncated error length: {}",
            error.len()
        );
        assert!(
            error.contains("stdout-head"),
            "missing stdout head: {error}"
        );
        assert!(
            error.contains("stderr-tail"),
            "missing stderr tail: {error}"
        );
    }

    #[test]
    fn external_tool_wait_observes_cancel_callback() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = ExternalToolConfig {
            name: "slow_tool".to_string(),
            description: "slow tool".to_string(),
            action_kind: ActionKind::Shell,
            command: platform_script(
                "printf before; sleep 5; printf after",
                "[Console]::Out.Write('before'); [Console]::Out.Flush(); Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
            ),
            schema: serde_json::json!({}),
        };
        let request = ToolRequest {
            id: "external-1".to_string(),
            name: ToolName::External("slow_tool".to_string()),
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let start = Instant::now();

        let result = execute_external_tool_with_policy_or_cancel(
            &config,
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            || start.elapsed() >= platform_delay(100, 1_500),
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "cancelled external tool should not wait for the shell timeout"
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("external tool 'slow_tool' cancelled"),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_tool_stdin_failure_reaps_started_process_before_returning() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("continued-after-stdin-failure");
        let config = ExternalToolConfig {
            name: "closed_stdin_tool".to_string(),
            description: "closes stdin before arguments arrive".to_string(),
            action_kind: ActionKind::Shell,
            command: format!("exec 0<&-; sleep 0.4; printf survived > {marker:?}"),
            schema: serde_json::json!({}),
        };
        let request = ToolRequest {
            id: "external-stdin-failure".to_string(),
            name: ToolName::External("closed_stdin_tool".to_string()),
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some("x".repeat(1024 * 1024)),
        };

        let result = execute_external_tool_with_policy(
            &config,
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(5),
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed to receive input")),
            "unexpected result: {result:?}"
        );
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            !marker.exists(),
            "external tool continued running after Orca recorded its terminal result"
        );
    }
}
