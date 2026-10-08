use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use orca_core::config::error_text::{data_error_text, syntax_error_text};
use orca_core::config::toml_text::crlf_to_lf;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};

use crate::process;

const MAX_EXTERNAL_TOOL_ENV_ARGS_BYTES: usize = 64 * 1024;

// Security: only loads from ORCA_HOME/tools/ (user-controlled), never from
// project-level directories, to prevent repo poisoning attacks.
pub fn default_tools_dir() -> Option<PathBuf> {
    orca_core::home::orca_home().map(|home| home.join("tools"))
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
            read_external_tool(&path, &content)
                .map_err(|warning| eprintln!("orca: warning: {warning}"))
                .ok()
        })
        .collect::<Vec<_>>();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// The tool the external tool file at `path` holds, `content`, or the
/// warning that says why it is not loaded. What is wrong is said by line and
/// column, or by key, and never by what the file holds: the warning lands
/// in logs and bug reports, and a tool's command can hold a token.
fn read_external_tool(path: &Path, content: &str) -> Result<ExternalToolConfig, String> {
    let failed =
        |what: String| format!("failed to parse external tool '{}': {what}", path.display());
    // The parser and the message get the same text (see `toml_text`).
    let content = crlf_to_lf(content);
    // Parsed to a table first, so that a value of the wrong type is named
    // by its key (see `data_error_text`).
    let table = content
        .parse::<toml::Table>()
        .map_err(|error| failed(syntax_error_text(error.message(), error.span(), &content)))?;
    let tool = toml::Value::Table(table)
        .try_into::<ExternalToolConfig>()
        .map_err(|error| failed(data_error_text(&error)))?;
    if !is_valid_tool_name(&tool.name) {
        return Err(format!(
            "ignoring external tool with invalid name '{}'",
            tool.name
        ));
    }
    Ok(tool)
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
    execute_external_tool_with_policy_or_cancel_with_profile(
        config,
        request,
        cwd,
        output_truncation,
        Some(shell_timeout),
        orca_core::capability::ExecutionProfile::TrustedHost,
        should_cancel,
    )
}

/// Runs an external tool. With no `shell_timeout` the tool runs until it exits
/// or is cancelled.
pub fn execute_external_tool_with_policy_or_cancel_with_profile(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Option<Duration>,
    execution_profile: orca_core::capability::ExecutionProfile,
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

    let capabilities = match config.action_kind {
        orca_core::approval_types::ActionKind::Read => {
            orca_core::capability::CapabilitySet::read_only()
        }
        orca_core::approval_types::ActionKind::Write
        | orca_core::approval_types::ActionKind::Shell => {
            orca_core::capability::CapabilitySet::workspace_write()
        }
        orca_core::approval_types::ActionKind::Network => orca_core::capability::CapabilitySet {
            read: true,
            write: false,
            metadata_write: false,
            network: true,
            shell: true,
            agent: false,
        },
        orca_core::approval_types::ActionKind::Agent => orca_core::capability::CapabilitySet {
            read: true,
            write: true,
            metadata_write: false,
            network: true,
            shell: true,
            agent: true,
        },
    };
    let (mut child, process_job, _receipt) = match process::spawn_with_capability_profile(
        command,
        format!("external:{}", config.name),
        cwd,
        orca_core::capability::CapabilityProcessClass::UserTrustedIntegration,
        capabilities,
        orca_core::capability::EnforcementState::Advisory,
        "external-tool-user-trusted",
        execution_profile,
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("external tool '{}' failed to start: {error}", config.name),
                None,
            );
        }
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(args.as_bytes())
    {
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

    let output = match process::wait_for_child_output_or_cancel(
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
        // Only a set timeout can expire.
        let shell_timeout = shell_timeout.unwrap_or_default();
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
    fn an_external_tool_file_reads_back_as_its_tool() {
        let content = r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { env = { type = "string", description = "environment" } }
"#;

        let tool = read_external_tool(Path::new("deploy.toml"), content).expect("loads");

        assert_eq!(tool.name, "deploy");
        assert_eq!(tool.description, "Deploy the current branch");
        assert_eq!(tool.action_kind, ActionKind::Write);
        assert_eq!(tool.command, "./scripts/deploy.sh");
        assert_eq!(
            tool.schema,
            serde_json::json!({"env": {"type": "string", "description": "environment"}})
        );
    }

    /// What is wrong with an external tool file is said by line and column,
    /// or by key, and never by what the file holds: the warning lands in
    /// logs and bug reports, and a tool's command can hold a token.
    #[test]
    fn an_external_tool_that_does_not_load_is_reported_without_its_values() {
        let path = Path::new("/home/me/.orca/tools/lookup.toml");
        let head = "name = \"lookup\"\ndescription = \"looks up\"\n";
        // (the file, how it is reported, what must not be shown)
        let cases = [
            (
                format!("{head}action_kind = \"read\"\ncommand = \"abc-SECRET\n"),
                "TOML syntax error at line 4, column 22: invalid basic string, expected `\"`",
                "abc-SECRET",
            ),
            (
                format!("{head}action_kind = \"read\"\ncommand = [\"abc-SECRET\"]\n"),
                "invalid type in `command`, expected a string",
                "abc-SECRET",
            ),
            (
                format!("{head}action_kind = \"abc-SECRET\"\ncommand = \"echo\"\n"),
                "unknown variant in `action_kind`, expected one of `read`, `write`, `network`, `agent`, `shell`",
                "abc-SECRET",
            ),
            (
                format!("{head}command = \"abc-SECRET\"\n"),
                "missing field `action_kind`",
                "abc-SECRET",
            ),
        ];
        for (content, reported, value) in cases {
            let warning = read_external_tool(path, &content).unwrap_err();

            assert_eq!(
                warning,
                format!(
                    "failed to parse external tool '{}': {reported}",
                    path.display()
                ),
                "{content}"
            );
            assert!(!warning.contains(value), "{content}: {warning}");
        }
    }

    /// A tool file saved with CRLF line ends reads as one saved with LF: the
    /// line breaks inside a multi-line string are LF, as they were with
    /// toml 0.8 (a command reaches the shell as written, without CRs), and a
    /// syntax error is at the same line and column.
    #[test]
    fn an_external_tool_file_with_crlf_line_ends_reads_like_one_with_lf() {
        let path = Path::new("/home/me/.orca/tools/deploy.toml");
        let lf = concat!(
            "name = \"deploy\"\n",
            "description = \"\"\"\n",
            "Deploys the branch\n",
            "to staging\"\"\"\n",
            "action_kind = \"write\"\n",
            "command = '''\n",
            "set -e\n",
            "./scripts/deploy.sh\n",
            "'''\n",
        );
        for content in [lf.to_string(), lf.replace('\n', "\r\n")] {
            let tool = read_external_tool(path, &content).expect("loads");

            assert_eq!(
                tool.description, "Deploys the branch\nto staging",
                "{content:?}"
            );
            assert_eq!(tool.command, "set -e\n./scripts/deploy.sh\n", "{content:?}");
        }

        let broken = "name = \"lookup\"\ndescription = \"looks up\"\naction_kind = \"read\"\ncommand = \"abc-SECRET\n";
        for content in [broken.to_string(), broken.replace('\n', "\r\n")] {
            let warning = read_external_tool(path, &content).unwrap_err();

            assert_eq!(
                warning,
                format!(
                    "failed to parse external tool '{}': TOML syntax error at line 4, column 22: invalid basic string, expected `\"`",
                    path.display()
                ),
                "{content:?}"
            );
        }
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
