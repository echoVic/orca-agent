use std::path::{Path, PathBuf};
use std::time::Duration;

use orca_core::tool_types::{ToolName, ToolResult};
use serde::Deserialize;

use crate::runtime_bash::{RuntimeBashInvocationContext, execute_bash_with_shell_session};
use crate::runtime_tool_call::{RuntimeNormalToolInvocation, RuntimeNormalToolWorkerContext};
use crate::shell_session::ShellTerminalMode;
use crate::terminal_service::{TerminalExecRequest, TerminalServiceOutput};

const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
const DEFAULT_WRITE_YIELD_TIME_MS: u64 = 250;
const DEFAULT_POLL_YIELD_TIME_MS: u64 = 5_000;
const MAX_YIELD_TIME_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 2_000;
const MAX_OUTPUT_TOKENS: usize = 20_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;

#[derive(Deserialize)]
struct ExecCommandArgs {
    cmd: String,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    tty: bool,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct WriteStdinArgs {
    session_id: String,
    #[serde(default)]
    chars: Option<String>,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub(crate) fn execute_runtime_normal_tool(
    invocation: &RuntimeNormalToolInvocation,
    context: &mut RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    if invocation.request.name == ToolName::Bash
        && let Some(task_registry) = invocation.task_registry.as_ref()
    {
        return execute_bash_with_shell_session(RuntimeBashInvocationContext {
            config: invocation.config.as_ref(),
            request: &invocation.request,
            cwd: &invocation.cwd,
            additional_roots: &invocation.additional_roots,
            output_truncation: invocation.output_truncation,
            shell_timeout_secs: invocation.shell_timeout_secs,
            task_registry,
            cancel: Some(context.cancel),
            permission_handler: context.permission_handler,
            permission_overlay: context.permission_overlay,
            output_handler: context.output_handler.take(),
        });
    }

    if invocation.request.name == ToolName::ExecCommand {
        return execute_command(invocation, context);
    }

    if invocation.request.name == ToolName::WriteStdin {
        return write_stdin(invocation, context);
    }

    orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
        &invocation.request,
        &invocation.cwd,
        &invocation.additional_roots,
        &invocation.mcp_registry,
        &invocation.external_tools,
        invocation.output_truncation,
        invocation.shell_timeout_secs,
        context.mcp_elicitation_handler,
        || context.cancel.is_cancelled(),
    )
}

fn execute_command(
    invocation: &RuntimeNormalToolInvocation,
    context: &RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    let args: ExecCommandArgs = match parse_arguments(invocation, "exec_command") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let command = invocation.request.target.as_deref().unwrap_or(&args.cmd);
    if command.trim().is_empty() {
        return ToolResult::invalid_input(&invocation.request, "cmd must not be empty");
    }
    let cwd = match resolve_workdir(&invocation.cwd, args.workdir.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let Some(service) = invocation.terminal_service.as_ref() else {
        return ToolResult::failed_before_start(
            &invocation.request,
            "exec_command requires a runtime-owned terminal service",
            None,
        );
    };
    let terminal = if args.tty {
        ShellTerminalMode::pty(Some(120), Some(30))
    } else {
        ShellTerminalMode::pipe()
    };
    let output = service.exec(
        TerminalExecRequest {
            command,
            cwd: &cwd,
            additional_roots: &invocation.additional_roots,
            config: invocation.config.as_ref(),
            permission_overlay: &invocation.permission_overlay,
            terminal,
            #[cfg(test)]
            sandbox_override: None,
        },
        yield_time(args.yield_time_ms, DEFAULT_EXEC_YIELD_TIME_MS),
        max_output_bytes(args.max_output_tokens),
        || context.cancel.is_cancelled(),
    );
    terminal_output_result(invocation, output)
}

fn write_stdin(
    invocation: &RuntimeNormalToolInvocation,
    context: &RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    let args: WriteStdinArgs = match parse_arguments(invocation, "write_stdin") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    if args.session_id.trim().is_empty() {
        return ToolResult::invalid_input(&invocation.request, "session_id must not be empty");
    }
    let Some(service) = invocation.terminal_service.as_ref() else {
        return ToolResult::failed_before_start(
            &invocation.request,
            "write_stdin requires a runtime-owned terminal service",
            None,
        );
    };
    let default_yield_time = if args.chars.is_some() {
        DEFAULT_WRITE_YIELD_TIME_MS
    } else {
        DEFAULT_POLL_YIELD_TIME_MS
    };
    let output = service.write_stdin(
        &args.session_id,
        args.chars.as_deref(),
        yield_time(args.yield_time_ms, default_yield_time),
        max_output_bytes(args.max_output_tokens),
        || context.cancel.is_cancelled(),
    );
    terminal_output_result(invocation, output)
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    invocation: &RuntimeNormalToolInvocation,
    tool_name: &str,
) -> Result<T, String> {
    let raw = invocation
        .request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| format!("missing {tool_name} arguments JSON"))?;
    serde_json::from_str(raw).map_err(|error| format!("invalid {tool_name} arguments: {error}"))
}

fn resolve_workdir(base: &Path, workdir: Option<&Path>) -> Result<PathBuf, String> {
    let cwd = match workdir {
        Some(workdir) if workdir.is_absolute() => workdir.to_path_buf(),
        Some(workdir) => base.join(workdir),
        None => base.to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err(format!(
            "workdir is not an existing directory: {}",
            cwd.display()
        ));
    }
    Ok(cwd)
}

fn yield_time(requested: Option<u64>, default_ms: u64) -> Duration {
    Duration::from_millis(requested.unwrap_or(default_ms).min(MAX_YIELD_TIME_MS))
}

fn max_output_bytes(requested_tokens: Option<usize>) -> usize {
    requested_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        .clamp(1, MAX_OUTPUT_TOKENS)
        .saturating_mul(APPROX_BYTES_PER_TOKEN)
}

fn terminal_output_result(
    invocation: &RuntimeNormalToolInvocation,
    output: std::io::Result<TerminalServiceOutput>,
) -> ToolResult {
    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            return ToolResult::invalid_input(&invocation.request, error.to_string());
        }
        Err(error) => {
            return ToolResult::failed_after_start(&invocation.request, error.to_string(), None);
        }
    };
    let truncated = output.truncated;
    match serde_json::to_string(&output) {
        Ok(output) => ToolResult::completed(&invocation.request, output, truncated),
        Err(error) => ToolResult::failed_after_start(
            &invocation.request,
            format!("failed to serialize terminal output: {error}"),
            None,
        ),
    }
}
