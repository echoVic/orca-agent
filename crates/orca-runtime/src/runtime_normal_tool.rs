use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orca_core::tool_types::{ToolName, ToolResult};
use serde::Deserialize;

use crate::network_proxy::RuntimeNetworkBlockDecision;
use crate::protocol::PermissionResponseDecision;
use crate::runtime_permission::{
    RuntimePermissionEvaluation, RuntimePermissionOrigin, RuntimePermissionPolicy,
};
use crate::runtime_state::PermissionRuntimeState;
use crate::runtime_tool_call::{RuntimeNormalToolInvocation, RuntimeNormalToolWorkerContext};
use crate::shell_session::ShellTerminalMode;
use crate::terminal_service::{
    ExecutionDeadline, TerminalExecRequest, TerminalServiceOutput, merge_terminal_output,
};

pub(crate) const DEFAULT_YIELD_TIME_MS: u64 = 1_000;
const MAX_YIELD_TIME_MS: u64 = 30_000;
const DEFAULT_WRITE_YIELD_TIME_MS: u64 = 250;
const DEFAULT_WAIT_TIME_MS: u64 = 30_000;
const MAX_WAIT_TIME_MS: u64 = 60_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 2_000;
const MAX_OUTPUT_TOKENS: usize = 20_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;
/// Polling cadence for `task_wait`. A wait is a subscription over the
/// supervisor's own reap cadence, not a busy loop; it returns as soon as the
/// observed state satisfies the condition.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    terminal: Option<String>,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    lifetime: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct TaskReadOutputArgs {
    task_id: String,
    #[serde(default)]
    cursor: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct TaskSendInputArgs {
    task_id: String,
    #[serde(default)]
    chars: Option<String>,
    #[serde(default)]
    eof: bool,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct TaskWaitArgs {
    task_ids: Vec<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub(crate) fn execute_runtime_normal_tool(
    invocation: &RuntimeNormalToolInvocation,
    context: &mut RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    match invocation.request.name {
        ToolName::Bash => return execute_bash(invocation, context),
        ToolName::TaskReadOutput => return read_task_output(invocation),
        ToolName::TaskSendInput => return send_task_input(invocation, context),
        ToolName::TaskWait => return wait_for_tasks(invocation, context),
        _ => {}
    }

    orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation_with_profile(
        &invocation.request,
        &invocation.cwd,
        &invocation.additional_roots,
        &invocation.mcp_registry,
        &invocation.external_tools,
        invocation.output_truncation,
        invocation.shell_timeout_secs,
        context.mcp_elicitation_handler,
        invocation.config.execution_profile,
        || context.cancel.is_cancelled(),
    )
}

/// Starts one command through the runtime-owned terminal service.
///
/// The call waits at most `yield_time_ms` and then returns. A command that is
/// still running keeps running, so the result is reported as `running` and
/// never as a completed command: there is no exit code yet and no success
/// claim. `timeout_ms` is the only caller-supplied process limit.
fn execute_bash(
    invocation: &RuntimeNormalToolInvocation,
    context: &mut RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    let args: BashArgs = match parse_arguments(invocation, "bash") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let command = args.command.trim();
    if command.is_empty() {
        return ToolResult::invalid_input(&invocation.request, "command must not be empty");
    }
    let cwd = match resolve_workdir(&invocation.cwd, args.workdir.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let terminal = match args.terminal.as_deref() {
        None | Some("pipe") => ShellTerminalMode::pipe(),
        Some("pty") => ShellTerminalMode::pty(Some(120), Some(30)),
        Some(other) => {
            return ToolResult::invalid_input(
                &invocation.request,
                format!("terminal must be \"pipe\" or \"pty\", got {other:?}"),
            );
        }
    };
    // `lifetime` changes ownership, never permission. Only the two documented
    // values are accepted so a typo cannot silently pick a different owner.
    let lifetime = match args.lifetime.as_deref() {
        None | Some("task") => crate::tasks::TaskLifetime::Task,
        Some("workspace") => crate::tasks::TaskLifetime::Workspace,
        Some(other) => {
            return ToolResult::invalid_input(
                &invocation.request,
                format!("lifetime must be \"task\" or \"workspace\", got {other:?}"),
            );
        }
    };
    // A zero deadline would mean both "expire immediately" and "no limit", so
    // it is rejected rather than guessed at.
    if args.timeout_ms == Some(0) {
        return ToolResult::invalid_input(
            &invocation.request,
            "timeout_ms must be a positive number of milliseconds; omit it for no extra deadline",
        );
    }
    if let Err(error) = check_workspace_lifetime(lifetime, &invocation.permission_overlay) {
        return ToolResult::invalid_input(&invocation.request, error);
    }
    let Some(service) = invocation.terminal_service.as_ref() else {
        return ToolResult::failed_before_start(
            &invocation.request,
            "bash requires a runtime-owned terminal service",
            None,
        );
    };
    // The effective execution deadline is the earliest limit anyone set: the
    // caller's `timeout_ms` and the administrator's command cap. A caller
    // request is never silently shortened without naming the source.
    let caller_deadline = args.timeout_ms.map(|timeout_ms| ExecutionDeadline {
        after: Duration::from_millis(timeout_ms),
        source: "caller timeout_ms",
    });
    let admin_deadline = (invocation.shell_timeout_secs > 0).then(|| ExecutionDeadline {
        after: Duration::from_secs(invocation.shell_timeout_secs),
        source: "administrator command cap",
    });
    let mut execution_deadline = match (caller_deadline, admin_deadline) {
        (Some(caller), Some(admin)) => Some(if admin.after < caller.after {
            admin
        } else {
            caller
        }),
        (caller, admin) => caller.or(admin),
    };
    if let Some(deadline) = invocation
        .owner_task_id
        .as_deref()
        .and_then(|id| {
            invocation
                .task_registry
                .as_ref()
                .and_then(|registry| registry.get(id))
        })
        .and_then(|record| record.deadline_at_ms)
    {
        let remaining = deadline.saturating_sub(chrono::Utc::now().timestamp_millis());
        if remaining <= 0 {
            return ToolResult::cancelled_before_start(
                &invocation.request,
                "ancestor execution deadline passed",
            );
        }
        let inherited = ExecutionDeadline {
            after: Duration::from_millis(remaining as u64),
            source: "ancestor execution deadline",
        };
        if execution_deadline
            .as_ref()
            .is_none_or(|current| inherited.after < current.after)
        {
            execution_deadline = Some(inherited);
        }
    }
    let wait = yield_time(args.yield_time_ms, DEFAULT_YIELD_TIME_MS);
    let max_output_bytes = max_command_output_bytes(invocation, args.max_output_tokens);
    let mut next_output = service.exec(
        TerminalExecRequest {
            lifetime,
            command,
            cwd: &cwd,
            additional_roots: &invocation.additional_roots,
            config: &invocation.config,
            permission_overlay: context.permission_overlay,
            terminal,
            execution_deadline,
            #[cfg(test)]
            sandbox_override: None,
        },
        wait,
        max_output_bytes,
        || context.cancel.is_cancelled(),
        &mut |chunk: &str| {
            if let Some(handler) = context.output_handler.as_deref_mut() {
                handler(chunk);
            }
        },
    );
    let mut aggregate = None;
    loop {
        let mut output = match next_output {
            Ok(output) => output,
            Err(error) => return terminal_output_result(invocation, Err(error)),
        };
        let network_block = output.network_block.take();
        let session_id = output.session_id.clone();
        let task_id = output.task_id.clone();
        merge_terminal_output(&mut aggregate, output);
        let Some(block) = network_block else {
            break;
        };
        match RuntimePermissionPolicy::network_block_evaluation(
            &invocation.request.id,
            RuntimePermissionOrigin::Bash,
            &block,
        ) {
            RuntimePermissionEvaluation::Deny { reason, .. } => {
                deny_blocked_network_request(service, &session_id, &task_id);
                return ToolResult::denied(&invocation.request, reason);
            }
            RuntimePermissionEvaluation::Request(decision) => {
                let Some(handler) = context.permission_handler else {
                    deny_blocked_network_request(service, &session_id, &task_id);
                    return ToolResult::denied(
                        &invocation.request,
                        decision
                            .request
                            .reason
                            .unwrap_or_else(|| "network permission required".to_string()),
                    );
                };
                let response = match PermissionRuntimeState.request_permission(
                    context.permission_overlay,
                    handler,
                    decision.into_request(),
                ) {
                    Ok(response) => response,
                    Err(error) => {
                        deny_blocked_network_request(service, &session_id, &task_id);
                        return ToolResult::failed_after_start(
                            &invocation.request,
                            error.to_string(),
                            None,
                        );
                    }
                };
                if response.decision == PermissionResponseDecision::Deny {
                    deny_blocked_network_request(service, &session_id, &task_id);
                    return ToolResult::denied(
                        &invocation.request,
                        "permission request denied".to_string(),
                    );
                }
                if let Err(error) = service
                    .resolve_network_permission(&session_id, RuntimeNetworkBlockDecision::Allow)
                {
                    let _ = service.stop_task(&task_id);
                    return ToolResult::failed_after_start(
                        &invocation.request,
                        error.to_string(),
                        None,
                    );
                }
                next_output = service.continue_session(
                    &session_id,
                    wait,
                    max_output_bytes,
                    || context.cancel.is_cancelled(),
                    &mut |chunk: &str| {
                        if let Some(handler) = context.output_handler.as_deref_mut() {
                            handler(chunk);
                        }
                    },
                );
            }
        }
    }
    terminal_output_result(invocation, Ok(aggregate))
}

fn deny_blocked_network_request(
    service: &crate::terminal_service::TerminalService,
    session_id: &str,
    task_id: &str,
) {
    let _ = service.resolve_network_permission(session_id, RuntimeNetworkBlockDecision::Deny);
    let _ = service.stop_task(task_id);
}

/// Reads already-produced output using a caller-owned cursor.
///
/// One tool serves every task kind. A shell command's output lives in the
/// terminal archive; a child agent's lives in its durable result. Which one
/// answers is a runtime detail, not something the caller chooses between.
fn read_task_output(invocation: &RuntimeNormalToolInvocation) -> ToolResult {
    let args: TaskReadOutputArgs = match parse_arguments(invocation, "task_read_output") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let max_output_bytes = max_output_bytes(args.max_output_tokens);
    if let Some(service) = invocation.terminal_service.as_ref() {
        // An explicit cursor is an idempotent page: it never advances the
        // task's own cursor, so two readers can read the same bytes and a
        // repeated cursor returns the same content instead of an empty read.
        let output = match args.cursor {
            Some(cursor) => service.read_output_at(&args.task_id, cursor, max_output_bytes),
            None => service.read_output(&args.task_id, max_output_bytes),
        };
        if let Ok(Some(_)) = &output {
            return terminal_output_result(invocation, output);
        }
        if let Err(error) = &output
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return terminal_output_result(invocation, output);
        }
    }
    read_registry_task(invocation, &args, max_output_bytes)
}

/// Reads a task the terminal service does not own, using the same cursor rules.
fn read_registry_task(
    invocation: &RuntimeNormalToolInvocation,
    args: &TaskReadOutputArgs,
    max_output_bytes: usize,
) -> ToolResult {
    let Some(registry) = invocation.task_registry.as_ref() else {
        return ToolResult::failed_before_start(
            &invocation.request,
            "task_read_output requires a runtime-owned task registry",
            None,
        );
    };
    let Some(view) = crate::task_view::TaskView::lookup(registry, &args.task_id) else {
        return ToolResult::invalid_input(
            &invocation.request,
            "unknown task: it was not started in this session, is no longer retained, or belongs to another owner",
        );
    };
    let cursor = args.cursor.unwrap_or(0);
    let mut payload = view.to_json();
    if view.task_type == orca_core::task_types::TaskType::Subagent {
        let limit = (max_output_bytes / APPROX_BYTES_PER_TOKEN)
            .clamp(1, crate::task_view::MAX_AGENT_RESULT_PAGE_CHARS);
        match crate::task_view::page_agent_result(registry, &args.task_id, cursor, limit) {
            Some(page) => {
                if let Some(object) = payload.as_object_mut() {
                    object.insert("result".to_string(), serde_json::json!(page.page));
                    object.insert("result_offset".to_string(), serde_json::json!(page.offset));
                    object.insert(
                        "result_total_chars".to_string(),
                        serde_json::json!(page.total_chars),
                    );
                    object.insert(
                        "next_cursor".to_string(),
                        match page.next_offset {
                            Some(next) => serde_json::json!(next),
                            None => serde_json::Value::Null,
                        },
                    );
                }
            }
            None => {
                return ToolResult::invalid_input(&invocation.request, "unknown task result");
            }
        }
    }
    let serialized = match serde_json::to_string(&payload) {
        Ok(serialized) => serialized,
        Err(error) => {
            return ToolResult::failed_after_start(
                &invocation.request,
                format!("failed to serialize task view: {error}"),
                None,
            );
        }
    };
    if view.is_terminal() {
        ToolResult::completed(&invocation.request, serialized, false)
    } else {
        ToolResult::running(&invocation.request, serialized, false)
    }
}

/// Writes to a running pty task's standard input.
fn send_task_input(
    invocation: &RuntimeNormalToolInvocation,
    context: &RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    let args: TaskSendInputArgs = match parse_arguments(invocation, "task_send_input") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    if !args.eof && args.chars.as_ref().is_none_or(|chars| chars.is_empty()) {
        return ToolResult::invalid_input(
            &invocation.request,
            "task_send_input requires chars, eof, or both",
        );
    }
    let Some(service) = invocation.terminal_service.as_ref() else {
        return ToolResult::failed_before_start(
            &invocation.request,
            "task_send_input requires a runtime-owned terminal service",
            None,
        );
    };
    let output = service.send_input(
        &args.task_id,
        args.chars.as_deref().unwrap_or(""),
        args.eof,
        yield_time(args.yield_time_ms, DEFAULT_WRITE_YIELD_TIME_MS),
        max_output_bytes(args.max_output_tokens),
        || context.cancel.is_cancelled(),
    );
    terminal_output_result(invocation, output)
}

/// Waits for a state change on one or more tasks without changing them.
///
/// Waiting is a subscription: on timeout every target keeps running, and the
/// result reports the current state plus `wait_elapsed` rather than an error.
fn wait_for_tasks(
    invocation: &RuntimeNormalToolInvocation,
    context: &RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    let args: TaskWaitArgs = match parse_arguments(invocation, "task_wait") {
        Ok(args) => args,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    if args.task_ids.is_empty() {
        return ToolResult::invalid_input(&invocation.request, "task_ids must not be empty");
    }
    let terminal_only = match args.until.as_deref() {
        None | Some("terminal") => true,
        Some("state_change") => false,
        Some(other) => {
            return ToolResult::invalid_input(
                &invocation.request,
                format!("until must be \"terminal\" or \"state_change\", got {other:?}"),
            );
        }
    };
    let any = match args.condition.as_deref() {
        None | Some("all") => false,
        Some("any") => true,
        Some(other) => {
            return ToolResult::invalid_input(
                &invocation.request,
                format!("condition must be any or all, got {other:?}"),
            );
        }
    };
    let max_output_bytes = max_output_bytes(args.max_output_tokens);
    let wait = Duration::from_millis(
        args.wait_ms
            .unwrap_or(DEFAULT_WAIT_TIME_MS)
            .min(MAX_WAIT_TIME_MS),
    );
    // A child records the wait intent now. Only the main loop may release its
    // lease, after all started calls have settled and a checkpoint is durable.
    if let (Some(registry), Some(owner)) = (&invocation.task_registry, &invocation.owner_task_id)
        && let Some(record) = registry.get(owner)
        && record.task_type == orca_core::task_types::TaskType::Subagent
    {
        let mut ancestor = Some(owner.clone());
        let mut forbidden = std::collections::HashSet::new();
        while let Some(id) = ancestor {
            if !forbidden.insert(id.clone()) {
                break;
            }
            ancestor = registry.get(&id).and_then(|record| record.parent_task_id);
        }
        if args.task_ids.iter().any(|id| forbidden.contains(id)) {
            return ToolResult::invalid_input(
                &invocation.request,
                "cannot wait on self or an ancestor",
            );
        }
        let observations = match observe_once(invocation, &args.task_ids, max_output_bytes) {
            Ok(observations) => observations,
            Err(error) => return ToolResult::invalid_input(&invocation.request, error),
        };
        let pending = crate::tasks::PendingTaskWait {
            result: None,
            tool_call_id: invocation.request.id.clone(),
            task_ids: args.task_ids.clone(),
            deadline_at_ms: chrono::Utc::now()
                .timestamp_millis()
                .saturating_add(wait.as_millis() as i64),
            any,
            state_change: !terminal_only,
            baseline: args
                .task_ids
                .iter()
                .map(|id| {
                    registry
                        .get(id)
                        .map(|r| r.publication_revision)
                        .unwrap_or(0)
                })
                .collect(),
        };
        if let Err(error) = registry.set_pending_wait(owner, Some(pending)) {
            return ToolResult::failed_before_start(&invocation.request, error, None);
        }
        return ToolResult::running(&invocation.request, serde_json::json!({
            "wait": "scheduled", "tasks": observations.into_iter().map(|o| o.output).collect::<Vec<_>>(),
            "message": "The runtime will wait after this tool boundary and deliver the observed result before the next model request."
        }).to_string(), false);
    }
    let (observations, return_reason) = match observe_until(
        invocation,
        &args.task_ids,
        wait,
        terminal_only,
        any,
        max_output_bytes,
        || context.cancel.is_cancelled(),
    ) {
        Ok(result) => result,
        Err(error) => return ToolResult::invalid_input(&invocation.request, error),
    };
    let all_terminal = observations.iter().all(|observation| observation.terminal);
    let payload = serde_json::json!({
        "wait": return_reason,
        "return_reason": return_reason,
        "tasks": observations
            .iter()
            .map(|observation| observation.output.clone())
            .collect::<Vec<_>>(),
    });
    match serde_json::to_string(&payload) {
        Ok(output) => {
            if all_terminal {
                ToolResult::completed(&invocation.request, output, false)
            } else {
                ToolResult::running(&invocation.request, output, false)
            }
        }
        Err(error) => ToolResult::failed_after_start(
            &invocation.request,
            format!("failed to serialize task_wait result: {error}"),
            None,
        ),
    }
}

struct TaskObservation {
    output: serde_json::Value,
    terminal: bool,
    progress: (String, u64, bool),
}

fn observe_once(
    invocation: &RuntimeNormalToolInvocation,
    task_ids: &[String],
    max_output_bytes: usize,
) -> Result<Vec<TaskObservation>, String> {
    task_ids
        .iter()
        .map(|task_id| {
            if let Some(service) = &invocation.terminal_service {
                match service.read_output_at(task_id, 0, max_output_bytes) {
                    Ok(Some(output)) => {
                        return Ok(TaskObservation {
                            terminal: output.status != "running",
                            progress: (
                                output.status.to_owned(),
                                output.output_bytes_total as u64,
                                output.eof,
                            ),
                            output: serde_json::to_value(output)
                                .map_err(|error| error.to_string())?,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("cannot observe task {task_id}: {error}")),
                }
            }
            let registry = invocation
                .task_registry
                .as_ref()
                .ok_or_else(|| format!("unknown task {task_id}"))?;
            let record = registry
                .get(task_id)
                .ok_or_else(|| format!("unknown task {task_id}"))?;
            let view = crate::task_view::TaskView::of_task(registry, &record);
            let mut output = view.to_json();
            if let Some(page) = crate::task_view::page_agent_result(
                registry,
                task_id,
                0,
                (max_output_bytes / APPROX_BYTES_PER_TOKEN)
                    .clamp(1, crate::task_view::MAX_AGENT_RESULT_PAGE_CHARS),
            ) {
                output["result"] = serde_json::json!(page.page);
                output["next_cursor"] = serde_json::json!(page.next_offset);
            }
            Ok(TaskObservation {
                terminal: view.is_terminal(),
                progress: (
                    view.execution.as_str().to_owned(),
                    record.publication_revision,
                    view.is_terminal(),
                ),
                output,
            })
        })
        .collect()
}

fn observe_until(
    invocation: &RuntimeNormalToolInvocation,
    task_ids: &[String],
    wait: Duration,
    terminal_only: bool,
    any: bool,
    max_output_bytes: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<(Vec<TaskObservation>, &'static str), String> {
    let deadline = Instant::now() + wait;
    let mut observations = observe_once(invocation, task_ids, max_output_bytes)?;
    let baseline: Vec<_> = observations
        .iter()
        .map(|item| item.progress.clone())
        .collect();
    loop {
        let terminal = if any {
            observations.iter().any(|item| item.terminal)
        } else {
            observations.iter().all(|item| item.terminal)
        };
        let changed = observations
            .iter()
            .zip(&baseline)
            .any(|(item, before)| &item.progress != before);
        let reason = if terminal {
            Some("target_reached_terminal")
        } else if !terminal_only && changed {
            Some("state_changed")
        } else if should_cancel() {
            Some("wait_cancelled")
        } else if Instant::now() >= deadline {
            Some("wait_elapsed")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Ok((observations, reason));
        }
        std::thread::sleep(
            WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
        observations = observe_once(invocation, task_ids, max_output_bytes)?;
    }
}

/// Rejects ownership transfer when it would carry a temporary grant past its
/// authorization.
///
/// A turn-scoped grant is temporary by construction. A workspace service
/// outlives the turn, so it must not inherit that grant; the caller either
/// starts the service without the extra grant or keeps the command for the
/// task.
fn check_workspace_lifetime(
    lifetime: crate::tasks::TaskLifetime,
    overlay: &crate::runtime_permission::TurnPermissionOverlay,
) -> Result<(), String> {
    if lifetime != crate::tasks::TaskLifetime::Workspace {
        return Ok(());
    }
    if overlay.additional_working_directories().is_empty()
        && overlay.metadata_writable_directories().is_empty()
        && overlay.network_domain_permissions().is_empty()
    {
        return Ok(());
    }
    Err(
        "lifetime \"workspace\" cannot carry this turn's temporary permission grants; \
         start the service without the extra grant, or keep it for the task"
            .to_string(),
    )
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
    let identity = orca_core::workspace_identity::WorkspaceIdentity::new(base)
        .map_err(|error| format!("invalid workspace root: {error:?}"))?;
    identity.resolve_cwd(&cwd).map_err(|error| match error {
        orca_core::workspace_identity::WorkspacePathError::OutsideWorkspace => {
            format!("workdir is outside the workspace: {}", cwd.display())
        }
        other => format!("workdir rejected: {other:?}"),
    })
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

/// The byte budget for one command result: the caller's request, capped by the
/// administrator's truncation policy.
///
/// A terminal task is read with a cursor, so a capped page is not lost data:
/// the result names the cursor that continues it. What it must not do is
/// ignore the configured policy and hand back an arbitrarily long page.
fn max_command_output_bytes(
    invocation: &RuntimeNormalToolInvocation,
    requested_tokens: Option<usize>,
) -> usize {
    let requested = max_output_bytes(requested_tokens);
    let policy = match invocation.output_truncation.normalized() {
        orca_core::tool_types::ToolOutputTruncation::Bytes { limit } => limit,
        orca_core::tool_types::ToolOutputTruncation::Tokens { limit } => {
            limit.saturating_mul(APPROX_BYTES_PER_TOKEN)
        }
    };
    requested.min(policy).max(1)
}

/// Projects a terminal observation into the model-facing result.
///
/// `return_reason` answers "why did this call return"; `termination_reason`
/// answers "why did the process end". They are never conflated, and a running
/// command is reported as running with no exit code.
fn terminal_output_result(
    invocation: &RuntimeNormalToolInvocation,
    output: std::io::Result<Option<TerminalServiceOutput>>,
) -> ToolResult {
    let output = match output {
        Ok(Some(output)) => output,
        Ok(None) => {
            return ToolResult::invalid_input(
                &invocation.request,
                "unknown task: it was not started in this session, is no longer retained, or belongs to another owner",
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            return ToolResult::invalid_input(&invocation.request, error.to_string());
        }
        Err(error) => {
            return ToolResult::failed_after_start(&invocation.request, error.to_string(), None);
        }
    };
    let running = output.status == "running";
    let return_reason = if running {
        "yield_elapsed"
    } else if output.deadline_reached {
        "deadline_exceeded"
    } else {
        "terminal_observed"
    };
    let payload = serde_json::json!({
        "task_id": output.task_id,
        "state": output.status,
        "return_reason": return_reason,
        "termination_reason": if running { None } else { Some(output.termination) },
        "exit_code": output.exit_code,
        "output": output.output,
        "next_cursor": output.next_output_offset,
        "output_gap": (output.omitted_prefix_bytes > 0).then(|| serde_json::json!({
            "omitted_prefix_bytes": output.omitted_prefix_bytes,
        })),
        "effective_deadline_ms": output.effective_deadline_ms,
        "deadline_source": output.deadline_source,
        "terminal": output.effective_terminal,
        "eof": output.eof,
    });
    let serialized = match serde_json::to_string(&payload) {
        Ok(serialized) => serialized,
        Err(error) => {
            return ToolResult::failed_after_start(
                &invocation.request,
                format!("failed to serialize terminal output: {error}"),
                None,
            );
        }
    };
    match output.status {
        "running" => ToolResult::running(&invocation.request, serialized, output.truncated),
        "completed" => ToolResult::completed(&invocation.request, serialized, output.truncated),
        "stopped" if output.termination == "cancelled" => {
            let mut result =
                ToolResult::cancelled(&invocation.request, serialized, output.exit_code);
            result.set_truncated(output.truncated);
            result
        }
        _ => {
            let mut result =
                ToolResult::failed_after_start(&invocation.request, serialized, output.exit_code);
            result.set_truncated(output.truncated);
            result
        }
    }
}

/// A normal-tool invocation wired to a real task registry and config.
#[cfg(test)]
pub(crate) fn normal_invocation_for_test(
    registry: &crate::tasks::TaskRegistry,
) -> RuntimeNormalToolInvocation {
    RuntimeNormalToolInvocation::snapshot(
        &crate::runtime_tool_call::tests::test_config(),
        &orca_core::tool_types::ToolRequest {
            id: "fixture".to_string(),
            name: orca_core::tool_types::ToolName::TaskList,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: None,
        },
        std::path::Path::new("."),
        &[],
        &orca_mcp::McpRegistry::default(),
        &[],
        orca_core::tool_types::ToolOutputTruncation::default(),
        0,
        Some(registry),
        crate::runtime_permission::TurnPermissionOverlay::default(),
    )
}

/// Test entry point: runs the real normal-tool dispatch for one request.
#[cfg(test)]
pub(crate) fn execute_runtime_normal_tool_for_test(
    invocation: &RuntimeNormalToolInvocation,
    request: &orca_core::tool_types::ToolRequest,
) -> ToolResult {
    let request_invocation = invocation.clone_for_request(request);
    let cancel = orca_core::cancel::CancelToken::new();
    let mut overlay = request_invocation.permission_overlay.clone();
    let mut context = RuntimeNormalToolWorkerContext {
        cancel: &cancel,
        permission_handler: None,
        mcp_elicitation_handler: None,
        output_handler: None,
        permission_overlay: &mut overlay,
    };
    execute_runtime_normal_tool(&request_invocation, &mut context)
}

#[cfg(test)]
mod tests {
    use super::{BashArgs, TaskWaitArgs, resolve_workdir, terminal_output_result};
    use crate::runtime_tool_call::RuntimeNormalToolInvocation;
    use crate::terminal_service::TerminalServiceOutput;
    use orca_core::tool_types::ToolStatus;

    fn invocation() -> RuntimeNormalToolInvocation {
        crate::runtime_tool_call::tests::normal_invocation(
            "bash-result",
            orca_core::tool_types::InterruptSemantics::WaitForTerminal,
        )
    }

    fn output(
        status: &'static str,
        termination: &'static str,
        exit_code: Option<i32>,
    ) -> TerminalServiceOutput {
        TerminalServiceOutput {
            session_id: "shell-1".to_string(),
            task_id: "cmd_1".to_string(),
            status,
            termination,
            output: "hi".to_string(),
            exit_code,
            truncated: false,
            omitted_prefix_bytes: 0,
            output_offset: 0,
            next_output_offset: 2,
            output_bytes_total: 2,
            eof: status != "running",
            requested_terminal: "pipe",
            effective_terminal: "pipe",
            deadline_reached: false,
            deadline_source: None,
            effective_deadline_ms: None,
            network_block: None,
        }
    }

    fn payload(result: &orca_core::tool_types::ToolResult) -> serde_json::Value {
        serde_json::from_str(
            result
                .output
                .as_deref()
                .or(result.error.as_deref())
                .expect("terminal payload"),
        )
        .expect("json payload")
    }

    #[test]
    fn a_running_command_is_never_reported_as_completed_or_successful() {
        let invocation = invocation();
        let result =
            terminal_output_result(&invocation, Ok(Some(output("running", "running", None))));

        assert_eq!(
            result.status,
            ToolStatus::Running,
            "a running command must not be reported as completed"
        );
        assert_eq!(
            result.exit_code, None,
            "a running process has no exit code to report"
        );
        let payload = payload(&result);
        assert_eq!(payload["state"], "running");
        assert_eq!(payload["return_reason"], "yield_elapsed");
        assert!(
            payload["termination_reason"].is_null(),
            "a running command has no termination reason"
        );
        assert_eq!(payload["task_id"], "cmd_1");
    }

    #[test]
    fn a_finished_command_reports_its_exit_code_and_termination() {
        let invocation = invocation();
        let result = terminal_output_result(
            &invocation,
            Ok(Some(output("completed", "exited", Some(0)))),
        );

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        let payload = payload(&result);
        assert_eq!(payload["return_reason"], "terminal_observed");
        assert_eq!(payload["termination_reason"], "exited");
    }

    #[test]
    fn a_deadline_stop_is_distinguishable_from_a_normal_exit() {
        let invocation = invocation();
        let mut timed_out = output("failed", "timed_out", Some(143));
        timed_out.deadline_reached = true;
        timed_out.deadline_source = Some("caller timeout_ms");
        timed_out.effective_deadline_ms = Some(1_500);
        let result = terminal_output_result(&invocation, Ok(Some(timed_out)));

        assert_eq!(result.status, ToolStatus::Failed);
        let payload = payload(&result);
        assert_eq!(payload["return_reason"], "deadline_exceeded");
        assert_eq!(payload["termination_reason"], "timed_out");
        assert_eq!(payload["deadline_source"], "caller timeout_ms");
        assert_eq!(payload["effective_deadline_ms"], 1_500);
        assert_ne!(payload["exit_code"], 0);
    }

    #[test]
    fn a_cancelled_command_is_not_reported_as_completed() {
        let invocation = invocation();
        let result = terminal_output_result(
            &invocation,
            Ok(Some(output("stopped", "cancelled", Some(137)))),
        );

        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(result.exit_code, Some(137));
    }

    #[test]
    fn a_failed_command_is_not_reported_as_completed() {
        let invocation = invocation();
        let result =
            terminal_output_result(&invocation, Ok(Some(output("failed", "exited", Some(2)))));

        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.exit_code, Some(2));
    }

    #[test]
    fn an_unknown_task_is_an_explicit_error_not_empty_output() {
        let invocation = invocation();
        let result = terminal_output_result(&invocation, Ok(None));

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown task")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    fn rejects_absolute_workdir_outside_base_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&base).expect("base");
        std::fs::create_dir_all(&outside).expect("outside");

        let result = resolve_workdir(&base, Some(&outside));

        assert!(result.is_err(), "outside workdir must be rejected");
    }

    #[test]
    fn a_workspace_service_cannot_inherit_a_temporary_permission_grant() {
        use crate::runtime_permission::TurnPermissionOverlay;
        use crate::tasks::TaskLifetime;

        let clean = TurnPermissionOverlay::default();
        assert!(
            super::check_workspace_lifetime(TaskLifetime::Workspace, &clean).is_ok(),
            "a service with no extra grant may outlive the task"
        );
        assert!(
            super::check_workspace_lifetime(TaskLifetime::Task, &clean).is_ok(),
            "task lifetime is always allowed"
        );

        let mut granted = TurnPermissionOverlay::default();
        granted.grant_additional_working_directory(std::env::temp_dir().join("granted"));
        let error = super::check_workspace_lifetime(TaskLifetime::Workspace, &granted)
            .expect_err("a temporary grant must not outlive its turn");
        assert!(error.contains("temporary permission grants"), "{error}");
        assert!(
            super::check_workspace_lifetime(TaskLifetime::Task, &granted).is_ok(),
            "the same grant is fine for a command that ends with the task"
        );
    }

    #[test]
    fn bash_defaults_leave_the_process_without_an_extra_deadline() {
        let args: BashArgs = serde_json::from_str(r#"{"command":"cargo test"}"#).unwrap();

        assert_eq!(args.command, "cargo test");
        assert_eq!(args.yield_time_ms, None, "yield falls back to the default");
        assert_eq!(
            args.timeout_ms, None,
            "no caller deadline means the command has no extra time limit"
        );
        assert_eq!(args.terminal, None, "pipe is the default terminal");
        assert_eq!(args.lifetime, None, "task is the default lifetime");
    }

    #[test]
    fn bash_accepts_the_documented_arguments() {
        let args: BashArgs = serde_json::from_str(
            r#"{"command":"gh run watch 1","workdir":"sub","terminal":"pty",
                "yield_time_ms":1000,"timeout_ms":600000,"lifetime":"workspace",
                "max_output_tokens":2000}"#,
        )
        .unwrap();

        assert_eq!(args.workdir.as_deref(), Some(std::path::Path::new("sub")));
        assert_eq!(args.terminal.as_deref(), Some("pty"));
        assert_eq!(args.yield_time_ms, Some(1_000));
        assert_eq!(args.timeout_ms, Some(600_000));
        assert_eq!(args.lifetime.as_deref(), Some("workspace"));
        assert_eq!(args.max_output_tokens, Some(2_000));
    }

    #[test]
    fn bash_rejects_a_zero_deadline_as_ambiguous() {
        // 0 cannot mean both "expire now" and "no limit", so the schema floor
        // is 1 and a zero reaches the handler as an explicit error.
        assert!(serde_json::from_str::<BashArgs>(r#"{"command":"x","timeout_ms":0}"#).is_ok());
        let args: BashArgs = serde_json::from_str(r#"{"command":"x","timeout_ms":0}"#).unwrap();
        assert_eq!(
            args.timeout_ms,
            Some(0),
            "the handler must reject this value"
        );
    }

    #[test]
    fn task_wait_parses_multiple_targets_and_rejects_negative_wait() {
        let args: TaskWaitArgs =
            serde_json::from_str(r#"{"task_ids":["cmd_1","cmd_2"],"until":"terminal"}"#).unwrap();
        assert_eq!(args.task_ids.len(), 2);
        assert_eq!(args.until.as_deref(), Some("terminal"));

        assert!(
            serde_json::from_str::<TaskWaitArgs>(r#"{"task_ids":["a"],"wait_ms":-1}"#).is_err()
        );
    }
    #[test]
    fn task_wait_observes_completed_agents_without_a_terminal_service() {
        let mut invocation = invocation();
        let registry = crate::tasks::TaskRegistry::new("wait-agent".into());
        let task = registry.create_subagent("inspect".into(), None);
        registry.complete(&task.id, "found it".into()).unwrap();
        invocation.task_registry = Some(registry);
        let (result, reason) = super::observe_until(
            &invocation,
            &[task.id],
            std::time::Duration::ZERO,
            true,
            false,
            2048,
            || false,
        )
        .unwrap();
        assert_eq!(reason, "target_reached_terminal");
        assert!(result[0].terminal);
        assert_eq!(result[0].output["result"], "found it");
    }

    #[test]
    fn task_wait_reports_missing_ids_and_actual_timeout() {
        let mut invocation = invocation();
        let registry = crate::tasks::TaskRegistry::new("wait-errors".into());
        let task = registry.create_subagent("pending".into(), None);
        invocation.task_registry = Some(registry);
        assert!(super::observe_once(&invocation, &["missing".into()], 2048).is_err());
        let (_, reason) = super::observe_until(
            &invocation,
            &[task.id],
            std::time::Duration::ZERO,
            false,
            false,
            2048,
            || false,
        )
        .unwrap();
        assert_eq!(reason, "wait_elapsed");
    }
}
