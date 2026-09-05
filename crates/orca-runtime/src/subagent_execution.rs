use std::io;
use std::path::Path;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types;
use orca_mcp::McpRegistry;

use crate::agent_child::ChildAgentExecutor;
use crate::agent_controller::AgentController;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus,
};
use crate::memory::MemoryBlock;
use crate::runtime_subagent_call::{RuntimeSubagentCallOutput, RuntimeSubagentInvocation};
use crate::runtime_tool_call::RuntimeToolCallRuntime;
use crate::session::record_tool_result_for_agent;
use crate::subagent::{self, SubagentIsolation, SubagentMode};
use crate::subagent_async_worker::{AsyncSubagentLaunchContext, launch_async_subagent};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
    validate_tool_invocation_with_external,
};
use crate::tool_turn::ToolTurnOutcome;
use crate::workflow::ipc::WorkflowIpcContext;

pub(crate) enum SubagentBatchRecordOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) struct RuntimeSubagentBatchToolTurnContext<'a, W: io::Write> {
    pub(crate) request: RuntimeSubagentBatchToolTurnRequest<'a>,
    pub(crate) io: RuntimeSubagentBatchToolTurnIo<'a, W>,
    pub(crate) services: RuntimeSubagentBatchToolTurnServices<'a>,
    pub(crate) runtime: RuntimeSubagentBatchToolTurnRuntime<'a>,
    pub(crate) child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct RuntimeSubagentBatchToolTurnRequest<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) tool_requests: &'a [tool_types::ToolRequest],
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    /// Per-child lease-derived budget bounds (parent remaining minus
    /// outstanding reservations), one per batch child in tool_requests order.
    pub(crate) child_budgets: Vec<Option<orca_core::budget::BudgetSpec>>,
}

pub(crate) struct RuntimeSubagentBatchToolTurnIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
}

pub(crate) struct RuntimeSubagentBatchToolTurnServices<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
}

pub(crate) struct RuntimeSubagentBatchToolTurnRuntime<'a> {
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) root_task_id: Option<&'a str>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) activity_ingress:
        Option<Arc<dyn crate::runtime_surface::RuntimeSubagentActivityIngress>>,
    pub(crate) permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    pub(crate) agent_controller: Option<Arc<AgentController>>,
}

struct SubagentBatchExecution {
    results: Vec<(RunStatus, tool_types::ToolResult)>,
    event_error: Option<io::Error>,
    /// Each child's consumed budget receipt, in child order.
    child_budget_usage: Vec<Option<orca_core::budget::BudgetUsage>>,
}

fn emit_batch_event<W: io::Write>(
    sink: &mut EventSink<W>,
    event: EventDraft,
    event_error: &mut Option<io::Error>,
) -> bool {
    match sink.emit(event) {
        Ok(()) => true,
        Err(error) => {
            if event_error.is_none() {
                *event_error = Some(error);
            }
            false
        }
    }
}

pub(crate) fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
        && config.budget.max_cost_usd_micros.is_none()
        && is_batchable_subagent_request(tool_request)
}

pub(crate) fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tool_types::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && is_batchable_subagent_request(&tool_requests[end]) {
        end += 1;
    }
    end
}

pub(crate) fn record_subagent_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<(RunStatus, tool_types::ToolResult)>,
    emit_deltas: bool,
) -> io::Result<SubagentBatchRecordOutcome> {
    let mut terminal = None;
    let mut record_error = None;
    for (status, result) in results {
        if let Err(error) = record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        ) && record_error.is_none()
        {
            record_error = Some(error);
        }

        if terminal.is_none()
            && matches!(
                status,
                RunStatus::ApprovalRequired | RunStatus::Failed | RunStatus::Cancelled
            )
        {
            terminal = Some((status, result.error.clone()));
        }
    }

    if let Some(error) = record_error {
        return Err(error);
    }

    Ok(match terminal {
        Some((status, error)) => SubagentBatchRecordOutcome::Return { status, error },
        None => SubagentBatchRecordOutcome::Continue,
    })
}

pub(crate) fn run_subagent_batch_tool_turn<W: io::Write>(
    context: RuntimeSubagentBatchToolTurnContext<'_, W>,
) -> (
    io::Result<ToolTurnOutcome>,
    Vec<Option<orca_core::budget::BudgetUsage>>,
) {
    let RuntimeSubagentBatchToolTurnContext {
        request,
        io,
        services,
        runtime,
        child_executor,
    } = context;
    let RuntimeSubagentBatchToolTurnRequest {
        config,
        cwd,
        tool_requests,
        subagent_depth,
        emit_deltas,
        child_budgets,
    } = request;
    let RuntimeSubagentBatchToolTurnIo {
        events,
        sink,
        conversation,
        history_writer,
        cost_tracker,
    } = io;
    let RuntimeSubagentBatchToolTurnServices {
        instructions,
        memory,
        mcp_registry,
        hooks,
    } = services;
    let RuntimeSubagentBatchToolTurnRuntime {
        cancel,
        task_registry,
        root_task_id,
        workflow_ipc,
        activity_ingress,
        permission_handler,
        agent_controller,
    } = runtime;
    let execution = execute_subagent_batch(
        config,
        cwd,
        events,
        sink,
        tool_requests,
        subagent_depth,
        emit_deltas,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cost_tracker,
        cancel,
        task_registry,
        root_task_id,
        workflow_ipc,
        child_executor,
        permission_handler,
        activity_ingress,
        agent_controller,
        &child_budgets,
    );

    let receipts = execution.child_budget_usage;
    let record_outcome = match record_subagent_batch_results(
        conversation,
        history_writer,
        execution.results,
        emit_deltas,
    ) {
        Ok(record_outcome) => record_outcome,
        // Child receipts survive persistence failures: the parent still
        // charges exactly what each child consumed.
        Err(error) => return (Err(error), receipts),
    };
    if let Some(error) = execution.event_error {
        return (Err(error), receipts);
    }

    match record_outcome {
        SubagentBatchRecordOutcome::Continue => (Ok(ToolTurnOutcome::Continue), receipts),
        SubagentBatchRecordOutcome::Return { status, error } => (
            Ok(ToolTurnOutcome::Return {
                status,
                error,
                terminal: None,
            }),
            receipts,
        ),
    }
}

fn is_batchable_subagent_request(tool_request: &tool_types::ToolRequest) -> bool {
    if tool_request.name != tool_types::ToolName::Subagent {
        return false;
    }
    let request = subagent::create_subagent_request(tool_request);
    request.mode == SubagentMode::Sync && request.isolation == SubagentIsolation::None
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tool_types::ToolRequest],
    subagent_depth: u32,
    emit_deltas: bool,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    root_task_id: Option<&str>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    child_executor: ChildAgentExecutor<io::Sink>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    activity_ingress: Option<Arc<dyn crate::runtime_surface::RuntimeSubagentActivityIngress>>,
    agent_controller: Option<Arc<AgentController>>,
    child_budgets: &[Option<orca_core::budget::BudgetSpec>],
) -> SubagentBatchExecution {
    #[cfg(test)]
    let activity_ingress =
        activity_ingress.or_else(|| Some(crate::runtime_subagent_call::test_activity_ingress()));
    // Each child's own lease bounds it (parent remaining minus outstanding
    // reservations); without a lease the child falls back to the parent
    // config's budget. The child_config is resolved per child below.
    let mut results: Vec<Option<(RunStatus, tool_types::ToolResult)>> =
        vec![None; tool_requests.len()];
    let mut child_budget_usage: Vec<Option<orca_core::budget::BudgetUsage>> =
        vec![None; tool_requests.len()];
    let mut runtime_outputs: Vec<Option<RuntimeSubagentCallOutput>> =
        (0..tool_requests.len()).map(|_| None).collect();
    let mut event_error = None;
    let tool_calls = RuntimeToolCallRuntime::for_normal_execution();
    let mut runtime = tool_calls.start_subagent_batch(cancel);
    let batch_id = uuid::Uuid::now_v7().to_string();

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if event_error.is_some() {
            let result = tool_types::ToolResult::failed_before_start(
                tool_request,
                "subagent dispatch stopped after event delivery failed",
                None,
            );
            if emit_deltas {
                emit_batch_event(
                    sink,
                    events.tool_call_requested(tool_request),
                    &mut event_error,
                );
                emit_batch_event(sink, events.tool_call_completed(&result), &mut event_error);
            }
            results[idx] = Some((RunStatus::Failed, result));
            continue;
        }
        if emit_deltas {
            let requested = events.tool_call_requested(tool_request);
            if !emit_batch_event(sink, requested, &mut event_error) {
                let result = tool_types::ToolResult::failed_before_start(
                    tool_request,
                    "subagent dispatch stopped because the requested event could not be delivered",
                    None,
                );
                emit_batch_event(sink, events.tool_call_completed(&result), &mut event_error);
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
        }

        let invocation = prepare_tool_invocation_with_external(
            tool_request,
            subagent_depth,
            config.subagents.max_depth,
            mcp_registry,
            &[],
        );
        if let Err(error) = validate_tool_invocation_with_external(&invocation, mcp_registry, &[]) {
            let result = error.into_result();
            if emit_deltas {
                emit_batch_event(sink, events.tool_call_completed(&result), &mut event_error);
            }
            results[idx] = Some((RunStatus::Failed, result));
            continue;
        }

        let pre_tool_outcome = match hooks.run_with_cancel_result(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
            cancel,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let (status, result) = match error {
                    HookRunError::Cancelled(_) => (
                        RunStatus::Cancelled,
                        tool_types::ToolResult::cancelled_before_start(
                            tool_request,
                            "the pre-tool hook was cancelled",
                        ),
                    ),
                    HookRunError::Failed(error) => (
                        RunStatus::Failed,
                        tool_types::ToolResult::failed_before_start(
                            tool_request,
                            format!("pre_tool_use hook blocked tool: {error}"),
                            None,
                        ),
                    ),
                };
                if emit_deltas {
                    emit_batch_event(sink, events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((status, result));
                continue;
            }
        };
        let invocation = match apply_pre_tool_outcome_with_external(
            invocation,
            &pre_tool_outcome,
            mcp_registry,
            &[],
        ) {
            Ok(invocation) => invocation,
            Err(error) => {
                let result = error.into_result();
                if emit_deltas {
                    emit_batch_event(sink, events.tool_call_completed(&result), &mut event_error);
                }
                results[idx] = Some((RunStatus::Failed, result));
                continue;
            }
        };

        let effective = invocation.effective;
        let request = subagent::with_delegation_snapshot(
            subagent::create_subagent_request(&effective),
            orca_core::config::DelegationSnapshot::from_config(config),
        );
        let description = request.description.clone();
        let tool_id = effective.id.clone();
        let child_config = match child_budgets.get(idx) {
            Some(Some(spec)) => apply_child_budget_spec(config, spec),
            _ => config.clone(),
        };
        let invocation = RuntimeSubagentInvocation::snapshot(
            effective,
            request,
            &child_config,
            cwd,
            instructions,
            memory,
            mcp_registry,
            hooks,
            workflow_ipc,
            subagent_depth + 1,
            child_executor,
            activity_ingress.clone(),
            permission_handler.clone(),
            task_registry,
            root_task_id,
            agent_controller.clone(),
            batch_id.clone(),
            u32::try_from(tool_requests.len()).unwrap_or(u32::MAX),
        );
        let admission = runtime.admit(idx, invocation, |task| {
            if !emit_deltas || agent_controller.is_some() {
                return Ok(());
            }
            sink.emit(task.attach_to_event(events.subagent_started(&tool_id, &description)))
        });
        if event_error.is_none() {
            event_error = admission.event_error;
        }
        if let Some((idx, output)) = admission.immediate {
            runtime_outputs[idx] = Some(output);
        }
    }

    for (idx, output) in runtime.finish() {
        runtime_outputs[idx] = Some(output);
    }
    for (idx, output) in runtime_outputs.into_iter().enumerate() {
        let Some(output) = output else {
            continue;
        };
        cost_tracker.merge(&output.cost_tracker);
        if emit_deltas && agent_controller.is_none() {
            emit_runtime_subagent_terminal(events, sink, &output, &mut event_error);
        }
        if emit_deltas {
            emit_batch_event(
                sink,
                events.tool_call_completed(&output.result),
                &mut event_error,
            );
            if let Err(error) = hooks.run(
                HookEvent::PostToolUse,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(&output.tool_request),
                    tool_result: Some(&output.result),
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            ) {
                emit_batch_event(
                    sink,
                    events.error(&format!("post_tool_use hook failed: {error}")),
                    &mut event_error,
                );
            }
        }
        let child_receipt = output.child_budget_usage;
        results[idx] = Some((output.status, output.result));
        child_budget_usage[idx] = child_receipt;
    }

    SubagentBatchExecution {
        results: results
            .into_iter()
            .map(|result| result.expect("each subagent batch item has a result"))
            .collect(),
        event_error,
        child_budget_usage,
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn execute_subagent_tool<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    emit_deltas: bool,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    root_task_id: Option<&str>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    child_executor: ChildAgentExecutor<io::Sink>,
    event_error: &mut Option<io::Error>,
    child_budget: Option<&orca_core::budget::BudgetSpec>,
) -> io::Result<(
    tool_types::ToolResult,
    Option<orca_core::budget::BudgetUsage>,
)> {
    #[cfg(test)]
    let test_ingress = Some(crate::runtime_subagent_call::test_activity_ingress());
    #[cfg(not(test))]
    let test_ingress = None;
    execute_subagent_tool_with_activity_ingress(
        config,
        cwd,
        events,
        sink,
        tool_request,
        subagent_depth,
        instructions,
        memory,
        mcp_registry,
        hooks,
        emit_deltas,
        cost_tracker,
        cancel,
        task_registry,
        root_task_id,
        workflow_ipc,
        child_executor,
        None,
        test_ingress,
        None,
        event_error,
        child_budget,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_subagent_tool_with_activity_ingress<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    emit_deltas: bool,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    root_task_id: Option<&str>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    child_executor: ChildAgentExecutor<io::Sink>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    activity_ingress: Option<Arc<dyn crate::runtime_surface::RuntimeSubagentActivityIngress>>,
    agent_controller: Option<Arc<AgentController>>,
    event_error: &mut Option<io::Error>,
    child_budget: Option<&orca_core::budget::BudgetSpec>,
) -> io::Result<(
    tool_types::ToolResult,
    Option<orca_core::budget::BudgetUsage>,
)> {
    let request = subagent::with_delegation_snapshot(
        subagent::create_subagent_request(tool_request),
        orca_core::config::DelegationSnapshot::from_config(config),
    );
    let description = request.description.clone();

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        emit_rejected_subagent_lifecycle(
            events,
            sink,
            tool_request,
            &description,
            &error,
            emit_deltas,
            event_error,
        );
        return Ok((
            tool_types::ToolResult::failed(tool_request, error, None),
            None,
        ));
    }

    if request.mode == SubagentMode::Async && config.budget.max_cost_usd_micros.is_some() {
        let error = "async subagents are unavailable while a cost budget is active; use sync mode so usage can be admitted and reconciled in the parent turn";
        emit_rejected_subagent_lifecycle(
            events,
            sink,
            tool_request,
            &description,
            error,
            emit_deltas,
            event_error,
        );
        return Ok((
            tool_types::ToolResult::failed(tool_request, error, None),
            None,
        ));
    }

    let has_parent_fence = activity_ingress
        .as_ref()
        .and_then(|ingress| ingress.parent_fence())
        .is_some();
    if request.mode == SubagentMode::Async && (agent_controller.is_none() || has_parent_fence) {
        let launch = launch_async_subagent(AsyncSubagentLaunchContext {
            config,
            cwd,
            tool_request,
            request,
            subagent_depth,
            task_registry,
            root_task_id,
            parent_fence: activity_ingress
                .as_ref()
                .and_then(|ingress| ingress.parent_fence()),
            activity_ingress: activity_ingress.clone(),
        });
        if emit_deltas && let Some(task) = launch.task.as_ref() {
            emit_batch_event(sink, events.task_status_updated(task), event_error);
        }
        return Ok((launch.result, None));
    }
    let child_config = config_for_remaining_subagent_budget(config, cost_tracker, child_budget);
    let invocation = RuntimeSubagentInvocation::snapshot(
        tool_request.clone(),
        request,
        &child_config,
        cwd,
        instructions,
        memory,
        mcp_registry,
        hooks,
        workflow_ipc,
        subagent_depth + 1,
        child_executor,
        activity_ingress,
        permission_handler,
        task_registry,
        root_task_id,
        agent_controller.clone(),
        tool_request.id.clone(),
        1,
    );
    let tool_calls = RuntimeToolCallRuntime::for_normal_execution();
    let execution = tool_calls.execute_subagent(invocation, cancel, |task| {
        if !emit_deltas || agent_controller.is_some() {
            return Ok(());
        }
        sink.emit(task.attach_to_event(events.subagent_started(&tool_request.id, &description)))
    });
    cost_tracker.merge(&execution.output.cost_tracker);
    if event_error.is_none() {
        *event_error = execution.event_error;
    }
    if emit_deltas && agent_controller.is_none() {
        emit_runtime_subagent_terminal(events, sink, &execution.output, event_error);
    }
    Ok((execution.output.result, execution.output.child_budget_usage))
}

fn config_for_remaining_subagent_budget(
    config: &RunConfig,
    cost_tracker: &CostTracker,
    child_budget: Option<&orca_core::budget::BudgetSpec>,
) -> RunConfig {
    let mut child_config = config.clone();
    if let Some(child_budget) = child_budget {
        // The parent operation's lease bounds this child precisely (parent
        // remaining minus outstanding reservations); every dimension comes
        // from the lease spec.
        child_config.budget = orca_core::config::BudgetConfig::from_spec(*child_budget);
        return child_config;
    }
    if let Some(max_cost_usd_micros) = config.budget.max_cost_usd_micros {
        let spent_micros = crate::cost::usd_to_micros(cost_tracker.totals().estimated_cost_usd);
        child_config.budget.max_cost_usd_micros =
            Some(max_cost_usd_micros.saturating_sub(spent_micros));
    }
    child_config
}

/// Applies a lease-derived budget spec to a child config copy.
fn apply_child_budget_spec(config: &RunConfig, spec: &orca_core::budget::BudgetSpec) -> RunConfig {
    let mut child_config = config.clone();
    child_config.budget = orca_core::config::BudgetConfig::from_spec(*spec);
    child_config
}

fn emit_runtime_subagent_terminal(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    output: &RuntimeSubagentCallOutput,
    event_error: &mut Option<io::Error>,
) {
    let Some(task) = output.task.as_ref() else {
        return;
    };
    emit_batch_event(
        sink,
        task.attach_to_event(events.subagent_completed(
            &output.tool_request.id,
            &output.description,
            output.status,
            output.event_output.as_deref(),
            output.event_error.as_deref(),
        )),
        event_error,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_rejected_subagent_lifecycle(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    description: &str,
    error: &str,
    emit_deltas: bool,
    event_error: &mut Option<io::Error>,
) {
    if !emit_deltas {
        return;
    }
    let mut lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{}", tool_request.id));
    let task = lifecycle.start_task(RuntimeTaskKind::Subagent).clone();
    emit_batch_event(
        sink,
        task.attach_to_event(events.subagent_started(&tool_request.id, description)),
        event_error,
    );
    let failed = lifecycle
        .finish_task(RunStatus::Failed)
        .cloned()
        .unwrap_or_else(|| task.with_status(RuntimeTaskStatus::Failed));
    emit_batch_event(
        sink,
        failed.attach_to_event(events.subagent_completed(
            &tool_request.id,
            description,
            RunStatus::Failed,
            None,
            Some(error),
        )),
        event_error,
    );
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::Command;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::memory::MemoryBlock;
    use crate::tasks::TaskRegistry;
    use orca_core::approval_types::ActionKind;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{OutputFormat, ProviderKind, RunConfig};
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::hook_types::{HookConfig, HookEvent};
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::Usage;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types;
    use orca_mcp::McpRegistry;

    use crate::agent_continuation::ContinuationStatus;
    use crate::child_agent_types::{
        SubagentActivityEvent, SubagentActivityOwner, SubagentActivityPayload,
    };
    use crate::protocol::{
        PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile,
    };
    use crate::runtime_permission::{
        RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
        RuntimePermissionResponse,
    };
    use crate::runtime_surface::{
        RuntimeSubagentActivityIngress, Sha256Digest, SurfaceTaskId, TaskRevision,
    };
    use crate::surface::SurfacePermissionOrigin;
    use crate::tool_turn::ToolTurnOutcome;

    fn platform_hook_script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    fn platform_test_deadline(unix_secs: u64, windows_secs: u64) -> Duration {
        Duration::from_secs(if cfg!(windows) {
            windows_secs
        } else {
            unix_secs
        })
    }

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: orca_core::approval_types::ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: orca_core::config::HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: orca_core::config::BudgetConfig::default(),
            mcp_servers: Vec::new(),
            external_tools: Vec::new(),
            hooks: Vec::new(),
            subagents,
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn subagent_request(id: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some(format!("inspect {id}")),
            raw_arguments: Some(
                serde_json::json!({
                    "description": format!("inspect {id}"),
                    "prompt": format!("inspect {id}")
                })
                .to_string(),
            ),
        }
    }

    fn history_writer_that_fails_on_append(
        label: &str,
    ) -> (tempfile::TempDir, crate::thread_store::SessionWriter) {
        let history = tempfile::tempdir().expect("history tempdir");
        let history_path = history.path().join("session.jsonl");
        let meta = crate::history::create_meta(history.path(), "mock", None, label);
        let mut meta_record = serde_json::to_value(meta)
            .expect("serialize history metadata")
            .as_object()
            .cloned()
            .expect("history metadata object");
        meta_record.insert("type".to_string(), serde_json::json!("session.meta"));
        std::fs::write(
            &history_path,
            format!("{}\n", serde_json::Value::Object(meta_record)),
        )
        .expect("seed history file");
        let mut writer =
            crate::thread_store::SessionWriter::append_to_existing(history_path.clone())
                .expect("open existing history");
        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        std::fs::remove_file(&history_path).expect("remove history file");
        std::fs::create_dir(&history_path).expect("replace history file with directory");
        (history, writer)
    }

    #[test]
    fn batch_plan_stops_at_async_request_boundary() {
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 3;
        let config = config(subagents);
        let async_request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("async")
        };
        let requests = vec![subagent_request("a"), async_request, subagent_request("b")];

        assert!(super::should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(super::collect_subagent_batch(&config, &requests, 0), 1);
    }

    #[test]
    fn budget_mode_disables_parallel_subagent_batching() {
        let subagents = SubagentConfig {
            max_parallel: 3,
            ..SubagentConfig::default()
        };
        let mut config = config(subagents);
        config.budget.max_cost_usd_micros = Some(1_000_000);
        let requests = [subagent_request("a"), subagent_request("b")];

        assert!(!super::should_run_subagent_batch(&config, &requests[0], 0));
    }

    #[test]
    fn sync_subagent_receives_only_remaining_aggregate_budget() {
        let mut config = config(SubagentConfig::default());
        config.budget.max_cost_usd_micros = Some(500_000);
        let mut cost_tracker = CostTracker::new(None);
        cost_tracker.add_usage(Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_tokens: 0,
        });

        let child_config =
            super::config_for_remaining_subagent_budget(&config, &cost_tracker, None);

        // 1M flash input tokens ≈ $0.14, leaving ≈ $0.36 of the $0.50 budget.
        let remaining = child_config
            .budget
            .max_cost_usd_micros
            .expect("remaining budget");
        assert!((remaining as f64 - 360_000.0).abs() < 2_000.0);
        assert_eq!(config.budget.max_cost_usd_micros, Some(500_000));
    }

    #[test]
    fn record_subagent_batch_results_records_tools_and_returns_failure() {
        let request = subagent_request("failed");
        let result = tool_types::ToolResult::failed(&request, "child failed", None);
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome = super::record_subagent_batch_results(
            &mut conversation,
            None,
            vec![(RunStatus::Failed, result)],
            true,
        )
        .expect("records subagent batch result");

        match outcome {
            super::SubagentBatchRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            super::SubagentBatchRecordOutcome::Continue => {
                panic!("failed subagent batch should request early return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
    }

    #[test]
    fn record_subagent_batch_results_records_executed_suffix_before_returning_first_failure() {
        let first_request = subagent_request("first");
        let failed_request = subagent_request("failed");
        let third_request = subagent_request("third");
        let mut conversation = orca_core::conversation::Conversation::new();

        let outcome = super::record_subagent_batch_results(
            &mut conversation,
            None,
            vec![
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &first_request,
                        "first completed".to_string(),
                        false,
                    ),
                ),
                (
                    RunStatus::Failed,
                    tool_types::ToolResult::failed(&failed_request, "child failed", None),
                ),
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &third_request,
                        "third completed".to_string(),
                        false,
                    ),
                ),
            ],
            false,
        )
        .expect("record complete subagent batch");

        assert!(matches!(
            outcome,
            super::SubagentBatchRecordOutcome::Return {
                status: RunStatus::Failed,
                error: Some(ref error),
            } if error == "child failed"
        ));
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    orca_core::conversation::Message::Tool { tool_call_id, .. } => {
                        tool_call_id.as_str()
                    }
                    _ => panic!("expected tool result"),
                })
                .collect::<Vec<_>>(),
            vec!["first", "failed", "third"]
        );
    }

    #[test]
    fn record_subagent_batch_results_keeps_live_terminals_after_history_failure() {
        let (_history, mut writer) =
            history_writer_that_fails_on_append("subagent batch history failure");
        let first = subagent_request("first");
        let second = subagent_request("second");
        let mut conversation = orca_core::conversation::Conversation::new();

        let error = match super::record_subagent_batch_results(
            &mut conversation,
            Some(&mut writer),
            vec![
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(&first, "first completed".to_string(), false),
                ),
                (
                    RunStatus::Success,
                    tool_types::ToolResult::completed(
                        &second,
                        "second completed".to_string(),
                        false,
                    ),
                ),
            ],
            true,
        ) {
            Err(error) => error,
            Ok(_) => panic!("history append must fail"),
        };

        assert!(error.raw_os_error().is_some());
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    orca_core::conversation::Message::Tool { tool_call_id, .. } => {
                        tool_call_id.as_str()
                    }
                    _ => panic!("expected tool result"),
                })
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn run_subagent_batch_tool_turn_executes_and_records_results() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("injected"), subagent_request("injected")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-turn".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();

        let (batch_result, _receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: fake_child_executor::<std::io::Sink>,
            });
        let outcome = batch_result.expect("run subagent batch tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], orca_core::conversation::Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "injected" && content.contains("injected child result"))
        );
        assert!(
            matches!(&conversation.messages[1], orca_core::conversation::Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "injected" && content.contains("injected child result"))
        );
    }

    fn fake_child_executor<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        assert_eq!(request.prompt, "inspect injected");
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("injected child result".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn silent_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("Mock silent final response.".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    struct CapturingPermissionHandler {
        requests: Arc<Mutex<Vec<RuntimePermissionRequest>>>,
    }

    #[derive(Debug, Default)]
    struct CapturingActivityIngress {
        events: Mutex<Vec<SubagentActivityEvent>>,
    }

    impl RuntimeSubagentActivityIngress for CapturingActivityIngress {
        fn owner(&self) -> SubagentActivityOwner {
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("permission-test-owner").unwrap(),
                task_revision: TaskRevision::try_new(1).unwrap(),
                authority_digest: Sha256Digest::new([7; 32]),
            }
        }

        fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct FailingTerminalActivityIngress {
        events: Mutex<Vec<SubagentActivityEvent>>,
    }

    impl RuntimeSubagentActivityIngress for FailingTerminalActivityIngress {
        fn owner(&self) -> SubagentActivityOwner {
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("failing-terminal-owner").unwrap(),
                task_revision: TaskRevision::try_new(1).unwrap(),
                authority_digest: Sha256Digest::new([6; 32]),
            }
        }

        fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()> {
            if matches!(event.payload, SubagentActivityPayload::Completed { .. }) {
                return Err(io::Error::other("simulated ambiguous terminal commit"));
            }
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ContinuationCheckingActivityIngress {
        registry: TaskRegistry,
        events: Mutex<Vec<SubagentActivityEvent>>,
        terminal_continuation_statuses: Mutex<Vec<Option<ContinuationStatus>>>,
    }

    impl ContinuationCheckingActivityIngress {
        fn new(registry: TaskRegistry) -> Self {
            Self {
                registry,
                events: Mutex::new(Vec::new()),
                terminal_continuation_statuses: Mutex::new(Vec::new()),
            }
        }
    }

    impl RuntimeSubagentActivityIngress for ContinuationCheckingActivityIngress {
        fn owner(&self) -> SubagentActivityOwner {
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("continuation-order-owner").unwrap(),
                task_revision: TaskRevision::try_new(1).unwrap(),
                authority_digest: Sha256Digest::new([8; 32]),
            }
        }

        fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()> {
            if matches!(event.payload, SubagentActivityPayload::Completed { .. }) {
                let status = self
                    .registry
                    .continuation_projection(event.task_id.as_str())
                    .ok()
                    .flatten()
                    .and_then(|projection| {
                        self.registry
                            .continuation_store()
                            .ok()?
                            .load_record(&projection.continuation_id)
                            .ok()?
                            .map(|record| record.status)
                    });
                self.terminal_continuation_statuses
                    .lock()
                    .unwrap()
                    .push(status);
            }
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    fn hosted_request(id: &str, description: &str, prompt: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": description,
                    "prompt": prompt,
                })
                .to_string(),
            ),
            ..subagent_request(id)
        }
    }

    fn execute_hosted_batch_for_test(
        config: RunConfig,
        cwd: std::path::PathBuf,
        requests: Vec<tool_types::ToolRequest>,
        cancel: CancelToken,
        task_registry: TaskRegistry,
        activity_ingress: Arc<dyn RuntimeSubagentActivityIngress>,
        controller: Arc<crate::agent_controller::AgentController>,
    ) -> super::SubagentBatchExecution {
        let mut events = EventFactory::new("hosted-cancellation-batch".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        super::execute_subagent_batch(
            &config,
            &cwd,
            &mut events,
            &mut sink,
            &requests,
            0,
            false,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            None,
            Some(activity_ingress),
            Some(controller),
            &[],
        )
    }

    impl RuntimePermissionRequestHandler for CapturingPermissionHandler {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> io::Result<RuntimePermissionResponse> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: false,
            })
        }
    }

    fn child_executor_observes_permission_handler<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        let handler = runtime
            .permission_handler
            .as_ref()
            .ok_or_else(|| io::Error::other("sync child permission handler missing"))?;
        let permission_request = RuntimePermissionRequest {
            id: request.prompt.clone(),
            reason: None,
            permissions: RequestPermissionProfile::default(),
            context: RuntimePermissionContext::foreground(SurfacePermissionOrigin::CommandExec),
        };
        handler.request_permissions(&permission_request)?;
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("permission handler observed".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn unexpected_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("budget-rejected async subagent must not start a child executor")
    }

    fn cancelled_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("child turn cancelled".to_string()),
            budget_usage: None,
        })
    }

    fn cancelling_child_executor<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        let marker = runtime
            .cwd
            .join(format!("{}.started", request.prompt.replace(' ', "-")));
        std::fs::write(marker, "started\n")?;
        runtime.cancel.cancel();
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("child cancelled parent batch".to_string()),
            budget_usage: None,
        })
    }

    fn delayed_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        std::thread::sleep(Duration::from_millis(250));
        std::fs::write(runtime.cwd.join("delayed-child-finished"), "finished\n")?;
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("finished".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn panic_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("child worker panic")
    }

    #[derive(Default)]
    struct FailThirdFlush {
        flushes: usize,
    }

    impl io::Write for FailThirdFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 3 {
                return Err(io::Error::other("event consumer disconnected"));
            }
            Ok(())
        }
    }

    #[test]
    fn subagent_batch_cancellation_stops_blocked_hook_and_unstarted_sibling() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-hook-cancel".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("first"), subagent_request("second")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: platform_hook_script(
                r#"if [ "$ORCA_TOOL_TARGET" = "inspect second" ]; then sleep 5; fi"#,
                "if ($env:ORCA_TOOL_TARGET -eq 'inspect second') { Start-Sleep -Seconds 5 }",
            ),
            tool: Some("subagent".to_string()),
        }]);
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-hook-cancel".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();
        let started = Instant::now();

        let (batch_result, _receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: false,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: cancelling_child_executor::<std::io::Sink>,
            });
        let outcome = batch_result.expect("cancel subagent batch");

        assert!(started.elapsed() < platform_test_deadline(2, 4));
        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert!(cwd.path().join("inspect-first.started").exists());
        assert!(!cwd.path().join("inspect-second.started").exists());
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[1],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Cancelled
                && terminal.started == tool_types::ToolInvocationStarted::No
        ));
    }

    #[test]
    fn subagent_batch_joins_started_worker_before_event_io_error_returns() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-event-error".to_string());
        let mut sink = EventSink::new(FailThirdFlush::default(), OutputFormat::Text);
        let requests = vec![subagent_request("first"), subagent_request("second")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-event-error".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();
        let started = Instant::now();

        let (batch_result, _receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: delayed_child_executor::<std::io::Sink>,
            });
        let error = match batch_result {
            Err(error) => error,
            Ok(_) => panic!("third event flush should fail after recording terminals"),
        };

        assert!(error.to_string().contains("event consumer disconnected"));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(cwd.path().join("delayed-child-finished").exists());
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[0],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Completed
        ));
        assert!(matches!(
            &conversation.messages[1],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Failed
                && terminal.started == tool_types::ToolInvocationStarted::No
        ));
    }

    #[test]
    fn subagent_batch_panic_is_indeterminate_and_closes_lifecycle_event() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-panic".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![subagent_request("panic")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-panic".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();

        let (batch_result, _receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: panic_child_executor::<std::io::Sink>,
            });
        let outcome = batch_result.expect("panic must become a terminal result");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                ..
            }
        ));
        assert!(matches!(
            &conversation.messages[0],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Indeterminate
                && terminal.started == tool_types::ToolInvocationStarted::Yes
        ));
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        let parsed = emitted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(parsed.iter().any(|event| {
            event["type"] == "subagent.completed"
                && event["payload"]["status"] == "failed"
                && event["payload"]["task"]["status"] == "failed"
        }));
        assert!(parsed.iter().any(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["status"] == "indeterminate"
        }));
    }

    fn remove_worktree_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        std::fs::remove_dir_all(runtime.cwd).expect("remove child worktree");
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("child completed before cleanup".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn isolated_subagent_cleanup_failure_returns_started_terminal() {
        let repo = tempfile::tempdir().expect("temp repo");
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
        run_git(repo.path(), &["config", "user.name", "Orca Test"]);
        std::fs::write(repo.path().join("tracked.txt"), "tracked\n").expect("write fixture");
        run_git(repo.path(), &["add", "tracked.txt"]);
        run_git(repo.path(), &["commit", "-m", "seed"]);

        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "cleanup failure",
                    "prompt": "cleanup failure",
                    "isolation": "worktree"
                })
                .to_string(),
            ),
            ..subagent_request("cleanup-failure")
        };
        let mut events = EventFactory::new("subagent-cleanup-failure".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-cleanup-failure".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            repo.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            remove_worktree_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("cleanup failure must return a tool terminal");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed to finish subagent worktree"))
        );
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        assert!(emitted.lines().any(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            event["type"] == "subagent.completed" && event["payload"]["status"] == "failed"
        }));
    }

    #[test]
    fn isolated_subagent_panic_cleans_registered_worktree_before_returning() {
        let repo = tempfile::tempdir().expect("temp repo");
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
        run_git(repo.path(), &["config", "user.name", "Orca Test"]);
        std::fs::write(repo.path().join("tracked.txt"), "tracked\n").expect("write fixture");
        run_git(repo.path(), &["add", "tracked.txt"]);
        run_git(repo.path(), &["commit", "-m", "seed"]);

        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "panic cleanup",
                    "prompt": "panic cleanup",
                    "isolation": "worktree"
                })
                .to_string(),
            ),
            ..subagent_request("panic-cleanup")
        };
        let mut events = EventFactory::new("subagent-panic-cleanup".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-panic-cleanup".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            repo.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            panic_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("panic must become a terminal after worktree cleanup");

        assert_eq!(result.status, tool_types::ToolStatus::Indeterminate);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Worktree cleaned:"))
        );
        let listed = Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("list worktrees");
        assert!(listed.status.success());
        let registered = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count();
        assert_eq!(registered, 1, "clean panic worktree must be removed");
    }

    #[test]
    fn subagent_batch_preserves_cancelled_child_terminals() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let requests = vec![
            subagent_request("cancelled-1"),
            subagent_request("cancelled-2"),
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-cancelled".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();

        let (batch_result, _receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: cancelled_child_executor::<std::io::Sink>,
            });
        let outcome = batch_result.expect("run cancelled subagent batch");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert_eq!(conversation.messages.len(), 2);
        for message in &conversation.messages {
            assert!(matches!(
                message,
                orca_core::conversation::Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == tool_types::ToolStatus::Cancelled
                    && terminal.started == tool_types::ToolInvocationStarted::Yes
            ));
        }
    }

    #[test]
    fn sync_subagent_uses_injected_child_executor() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-injected".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect injected",
                    "prompt": "inspect injected"
                })
                .to_string(),
            ),
            ..subagent_request("injected")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-injected".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            fake_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("injected child result")
        );
    }

    #[test]
    fn sync_subagent_child_executor_receives_scoped_permission_handler() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-permission-bridge".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect bridge",
                    "prompt": "child-permission-request"
                })
                .to_string(),
            ),
            ..subagent_request("permission-bridge")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-permission-bridge".to_string());
        let mut event_error = None;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let permission_handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync> =
            Arc::new(CapturingPermissionHandler {
                requests: Arc::clone(&captured),
            });
        let activity_ingress = Arc::new(CapturingActivityIngress::default());

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            child_executor_observes_permission_handler::<io::Sink>,
            Some(permission_handler),
            Some(activity_ingress.clone()),
            None,
            &mut event_error,
            None,
        )
        .expect("subagent permission bridge");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].context,
            RuntimePermissionContext::Child { .. }
        ));
        assert!(!activity_ingress.events.lock().unwrap().is_empty());
    }

    #[test]
    fn threaded_sync_subagent_without_surface_ingress_uses_legacy_observer_path() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("threaded-sync-missing-ingress".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("threaded-sync-missing-ingress");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("threaded-sync-missing-ingress".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "threaded-sync-missing-ingress".to_string(),
            "threaded-sync-missing-ingress".to_string(),
            0,
        ));
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            fake_child_executor::<io::Sink>,
            None,
            None,
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("threaded sync launch result");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert!(result.error.is_none());
        assert!(task_registry.list().is_empty());
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn hosted_async_subagent_still_requires_the_durable_worker_path() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-async-durable-route".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect later",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("hosted-async-durable-route")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-async-durable-route".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-async-durable-route".to_string(),
            "hosted-async-durable-route".to_string(),
            0,
        ));
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            fake_child_executor::<io::Sink>,
            None,
            None,
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("hosted async route result");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("persistent task ownership"))
        );
        assert!(task_registry.list().is_empty());
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn threaded_sync_launch_failure_finishes_the_canonical_registry_task() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_depth = 1;
        let config = config(subagents);
        let mut events = EventFactory::new("threaded-sync-launch-failure".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("threaded-sync-launch-failure");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("threaded-sync-launch-failure".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "threaded-sync-launch-failure".to_string(),
            "threaded-sync-launch-failure".to_string(),
            1,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            fake_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("threaded sync launch failure result");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        let tasks = task_registry.list();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, orca_core::task_types::TaskStatus::Failed);
        let activity = activity_ingress.events.lock().expect("surface activity");
        assert!(matches!(
            activity.first().map(|event| &event.payload),
            Some(crate::child_agent_types::SubagentActivityPayload::Started { .. })
        ));
        assert!(matches!(
            activity.last().map(|event| &event.payload),
            Some(
                crate::child_agent_types::SubagentActivityPayload::Completed {
                    status: crate::runtime_surface::SurfaceSubagentTerminalStatus::Failed,
                    ..
                }
            )
        ));
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn threaded_sync_registry_stop_interrupts_the_hosted_child() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let cwd_path = cwd.path().to_path_buf();
        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "wait until stopped",
                    "prompt": "mock_stream_delay_ms 30000"
                })
                .to_string(),
            ),
            ..subagent_request("threaded-sync-registry-stop")
        };
        let task_registry = TaskRegistry::new("threaded-sync-registry-stop".to_string());
        let worker_registry = task_registry.clone();
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "threaded-sync-registry-stop".to_string(),
            "threaded-sync-registry-stop".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let parent_cancel = CancelToken::new();
        let cleanup_cancel = parent_cancel.clone();
        let (result_tx, result_rx) = mpsc::channel();

        let worker = thread::spawn(move || {
            let mut events = EventFactory::new("threaded-sync-registry-stop".to_string());
            let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
            let instructions = ProjectInstructions::default();
            let memory = MemoryBlock::default();
            let mcp_registry = McpRegistry::default();
            let hooks = HookRunner::default();
            let mut cost_tracker = CostTracker::new(None);
            let mut event_error = None;
            let result = super::execute_subagent_tool_with_activity_ingress(
                &config,
                &cwd_path,
                &mut events,
                &mut sink,
                &request,
                0,
                &instructions,
                &memory,
                &mcp_registry,
                &hooks,
                false,
                &mut cost_tracker,
                &parent_cancel,
                &worker_registry,
                None,
                None,
                fake_child_executor::<io::Sink>,
                None,
                Some(activity_ingress),
                Some(controller),
                &mut event_error,
                None,
            )
            .expect("threaded sync stop result")
            .0;
            let _ = result_tx.send(result);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let task_id = loop {
            if let Some(task) = task_registry.list().into_iter().next()
                && task.status == orca_core::task_types::TaskStatus::Running
            {
                break task.id;
            }
            assert!(
                Instant::now() < deadline,
                "threaded child never reached running"
            );
            thread::sleep(Duration::from_millis(10));
        };
        task_registry
            .request_stop(&task_id)
            .expect("request registry stop");

        let (stopped_by_registry, result) = match result_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => (true, result),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cleanup_cancel.cancel();
                (
                    false,
                    result_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("parent cancellation must clean up delayed child"),
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("threaded child result channel disconnected")
            }
        };
        worker.join().expect("join threaded child caller");
        host.shutdown().expect("shutdown runtime host");

        assert!(
            stopped_by_registry,
            "request_stop(task_id) did not interrupt the hosted child"
        );
        assert_eq!(result.status, tool_types::ToolStatus::Cancelled);
        assert_eq!(
            task_registry.get(&task_id).expect("registry task").status,
            orca_core::task_types::TaskStatus::Stopped
        );
    }

    #[test]
    fn stopping_one_hosted_sibling_does_not_cancel_the_other_or_the_parent() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let release_marker = cwd.path().join("release-sibling");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let requests = vec![
            hosted_request(
                "stop-one",
                "stop only this child",
                "mock_stream_delay_ms 10000",
            ),
            hosted_request(
                "finish-one",
                "finish sibling",
                &format!("mock_stream_release_marker {}", release_marker.display()),
            ),
        ];
        let task_registry = TaskRegistry::new("hosted-independent-cancel".to_string());
        let worker_registry = task_registry.clone();
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-independent-cancel".to_string(),
            "hosted-independent-cancel".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let parent_cancel = CancelToken::new();
        let worker_cancel = parent_cancel.clone();
        let cwd_path = cwd.path().to_path_buf();
        let (result_tx, result_rx) = mpsc::channel();

        let worker = thread::spawn(move || {
            let result = execute_hosted_batch_for_test(
                config,
                cwd_path,
                requests,
                worker_cancel,
                worker_registry,
                activity_ingress,
                controller,
            );
            let _ = result_tx.send(result);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let stopped_task_id = loop {
            let tasks = task_registry.list();
            if tasks.len() == 2
                && tasks
                    .iter()
                    .all(|task| task.status == orca_core::task_types::TaskStatus::Running)
            {
                break tasks
                    .iter()
                    .find(|task| task.description == "stop only this child")
                    .expect("stopped sibling task")
                    .id
                    .clone();
            }
            assert!(Instant::now() < deadline, "siblings never reached running");
            thread::sleep(Duration::from_millis(10));
        };
        task_registry
            .request_stop(&stopped_task_id)
            .expect("stop one sibling");
        thread::sleep(Duration::from_millis(100));
        std::fs::write(&release_marker, "release\n").expect("release sibling");

        let execution = match result_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(execution) => execution,
            Err(error) => {
                parent_cancel.cancel();
                panic!("hosted sibling batch did not finish: {error}");
            }
        };
        worker.join().expect("join hosted batch");
        host.shutdown().expect("shutdown runtime host");

        assert!(!parent_cancel.is_cancelled());
        assert_eq!(execution.results[0].0, RunStatus::Cancelled);
        assert_eq!(
            execution.results[0].1.status,
            tool_types::ToolStatus::Cancelled
        );
        assert_eq!(execution.results[1].0, RunStatus::Success);
        assert_eq!(
            execution.results[1].1.status,
            tool_types::ToolStatus::Completed
        );
    }

    #[test]
    fn parent_cancellation_propagates_to_every_hosted_child() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let requests = vec![
            hosted_request("cancel-a", "cancel child a", "mock_stream_delay_ms 10000"),
            hosted_request("cancel-b", "cancel child b", "mock_stream_delay_ms 10000"),
        ];
        let task_registry = TaskRegistry::new("hosted-parent-cancel".to_string());
        let worker_registry = task_registry.clone();
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-parent-cancel".to_string(),
            "hosted-parent-cancel".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let parent_cancel = CancelToken::new();
        let worker_cancel = parent_cancel.clone();
        let cwd_path = cwd.path().to_path_buf();
        let (result_tx, result_rx) = mpsc::channel();

        let worker = thread::spawn(move || {
            let result = execute_hosted_batch_for_test(
                config,
                cwd_path,
                requests,
                worker_cancel,
                worker_registry,
                activity_ingress,
                controller,
            );
            let _ = result_tx.send(result);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let tasks = task_registry.list();
            if tasks.len() == 2
                && tasks
                    .iter()
                    .all(|task| task.status == orca_core::task_types::TaskStatus::Running)
            {
                break;
            }
            assert!(Instant::now() < deadline, "children never reached running");
            thread::sleep(Duration::from_millis(10));
        }
        parent_cancel.cancel();

        let execution = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("parent cancellation must settle every child");
        worker.join().expect("join hosted batch");
        host.shutdown().expect("shutdown runtime host");

        assert!(execution.results.iter().all(|(status, result)| {
            *status == RunStatus::Cancelled && result.status == tool_types::ToolStatus::Cancelled
        }));
    }

    #[test]
    fn hosted_sync_schema_request_uses_the_observable_runtime_route() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-schema-route".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "schema child",
                    "prompt": "mock_silent_final",
                    "schema": { "type": "number" }
                })
                .to_string(),
            ),
            ..subagent_request("hosted-schema-route")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-schema-route".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-schema-route".to_string(),
            "hosted-schema-route".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("hosted schema result");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        let activity = activity_ingress.events.lock().unwrap();
        assert!(activity.iter().any(|event| {
            matches!(
                event.payload,
                SubagentActivityPayload::ChildThreadBound { .. }
            )
        }));
        assert_eq!(
            activity
                .iter()
                .filter_map(|event| match event.payload {
                    SubagentActivityPayload::Completed { ref status, .. } => Some(status),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![&crate::runtime_surface::SurfaceSubagentTerminalStatus::Failed]
        );
        assert_eq!(
            task_registry.list()[0].status,
            orca_core::task_types::TaskStatus::Failed
        );
    }

    #[test]
    fn hosted_sync_resume_rejects_before_child_launch() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-resume-rejection".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "resume child",
                    "prompt": "mock_silent_final",
                    "resume_from": "missing-continuation"
                })
                .to_string(),
            ),
            ..subagent_request("hosted-resume-rejection")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-resume-rejection".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-resume-rejection".to_string(),
            "hosted-resume-rejection".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("hosted resume rejection");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::No
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| { error.contains("hosted_sync_resume_unsupported") })
        );
        assert!(task_registry.list().is_empty());
        assert!(activity_ingress.events.lock().unwrap().is_empty());
    }

    #[test]
    fn hosted_surface_terminal_observes_committed_continuation_terminal() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-terminal-order".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = hosted_request(
            "hosted-terminal-order",
            "ordered terminal child",
            "mock_silent_final",
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-terminal-order".to_string());
        let activity_ingress = Arc::new(ContinuationCheckingActivityIngress::new(
            task_registry.clone(),
        ));
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            silent_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            None,
            &mut event_error,
            None,
        )
        .expect("ordered terminal result");
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(
            activity_ingress
                .terminal_continuation_statuses
                .lock()
                .unwrap()
                .as_slice(),
            &[Some(ContinuationStatus::Completed)]
        );
        assert_eq!(
            activity_ingress
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| {
                    matches!(event.payload, SubagentActivityPayload::Completed { .. })
                })
                .count(),
            1
        );
    }

    #[test]
    fn hosted_terminal_commit_failure_returns_indeterminate_result() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-terminal-failure".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = hosted_request(
            "hosted-terminal-failure",
            "terminal failure child",
            "mock_silent_final",
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-terminal-failure".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host.handle(),
            "hosted-terminal-failure".to_string(),
            "hosted-terminal-failure".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(FailingTerminalActivityIngress::default());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("hosted terminal failure result");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(result.status, tool_types::ToolStatus::Indeterminate);
        assert!(result.error.as_deref().is_some_and(|error| {
            error.contains("surface terminal commit failed")
                && error.contains("Inspect external state before retrying")
        }));
        assert_eq!(
            activity_ingress
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event.payload, SubagentActivityPayload::Completed { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn hosted_child_thread_binding_uses_the_canonical_task_identity() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("hosted-canonical-binding".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = hosted_request(
            "tool-call-is-not-task-id",
            "canonical binding child",
            "mock_silent_final",
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("hosted-canonical-binding".to_string());
        let host = crate::runtime_host::RuntimeHost::start().expect("start runtime host");
        let host_handle = host.handle();
        let controller = Arc::new(crate::agent_controller::AgentController::new(
            host_handle.clone(),
            "hosted-canonical-binding".to_string(),
            "hosted-canonical-binding".to_string(),
            0,
        ));
        let activity_ingress = Arc::new(CapturingActivityIngress::default());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool_with_activity_ingress(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            false,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            None,
            Some(activity_ingress.clone()),
            Some(controller),
            &mut event_error,
            None,
        )
        .expect("hosted canonical binding result");

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let tasks = task_registry.list();
        assert_eq!(tasks.len(), 1);
        let canonical_task_id = tasks[0].id.as_str();
        assert_ne!(canonical_task_id, request.id);
        let activity = activity_ingress.events.lock().unwrap();
        assert!(activity.iter().all(|event| {
            event.task_id.as_str() == canonical_task_id
                && event.subagent_id.as_str() == canonical_task_id
        }));
        let child_thread_id = activity
            .iter()
            .find_map(|event| match &event.payload {
                SubagentActivityPayload::ChildThreadBound { thread_id } => {
                    Some(uuid::Uuid::from_bytes(*thread_id.as_bytes()).to_string())
                }
                _ => None,
            })
            .expect("typed child thread binding");
        drop(activity);
        let child = host_handle
            .resolve_live_thread(&child_thread_id)
            .expect("bound child thread remains live");
        assert_eq!(child.parent_thread_id(), Some("hosted-canonical-binding"));
        assert_eq!(
            child
                .snapshot()
                .expect("child transcript snapshot")
                .session_id(),
            Some(child_thread_id.as_str())
        );
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn sync_subagent_cancelled_before_admission_never_starts_child_or_lifecycle() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-pre-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("pre-cancelled");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        cancel.cancel();
        let task_registry = TaskRegistry::new("subagent-pre-cancelled".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("pre-cancelled subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Cancelled);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::No
        );
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        assert!(
            !emitted.lines().any(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                matches!(
                    event["type"].as_str(),
                    Some("subagent.started" | "subagent.completed")
                )
            }),
            "a child cancelled before admission must not publish a subagent lifecycle"
        );
    }

    #[test]
    fn sync_subagent_worker_panic_becomes_indeterminate_terminal() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-single-panic".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("panic");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-single-panic".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            panic_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("panicking subagent must return a terminal");

        assert_eq!(result.status, tool_types::ToolStatus::Indeterminate);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Inspect external state before retrying"))
        );
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        assert_eq!(
            emitted
                .lines()
                .filter(|line| {
                    let event: serde_json::Value = serde_json::from_str(line).unwrap();
                    event["type"] == "subagent.completed"
                })
                .count(),
            1
        );
    }

    #[test]
    fn budget_mode_rejects_async_subagent_before_task_launch() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut config = config(SubagentConfig::default());
        config.budget.max_cost_usd_micros = Some(1_000_000);
        let mut events = EventFactory::new("subagent-budget-async".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect later",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
            ..subagent_request("budget-async")
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-budget-async".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            unexpected_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("budget-rejected subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cost budget is active"))
        );
        assert!(task_registry.list().is_empty());
        assert_eq!(cost_tracker.totals(), Default::default());
    }

    #[test]
    fn sync_subagent_preserves_cancelled_child_terminal() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("subagent-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = subagent_request("cancelled");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-cancelled".to_string());
        let mut event_error = None;

        let (result, _receipt) = super::execute_subagent_tool(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &request,
            0,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            true,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            None,
            None,
            cancelled_child_executor::<io::Sink>,
            &mut event_error,
            None,
        )
        .expect("cancelled subagent tool");

        assert_eq!(result.status, tool_types::ToolStatus::Cancelled);
        assert_eq!(
            result.terminal().started,
            tool_types::ToolInvocationStarted::Yes
        );
        let error = result.error.as_deref().expect("cancelled child error");
        assert!(error.starts_with("Subagent status: Cancelled\n\nchild turn cancelled"));
        assert!(error.contains("[agent_continuation]"));
        assert!(error.contains("resume_from="));
    }

    fn receipt_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("finished".to_string()),
            error: None,
            budget_usage: Some(orca_core::budget::BudgetUsage {
                turns: 2,
                tool_calls: 3,
                cost_usd_micros: 700,
                wall_time_ms: 0,
            }),
        })
    }

    #[test]
    fn batch_persistence_failure_preserves_completed_child_receipts() {
        let cwd = tempfile::tempdir().expect("temp cwd");
        let mut subagents = SubagentConfig::default();
        subagents.max_parallel = 2;
        let config = config(subagents);
        let mut events = EventFactory::new("subagent-batch-receipt-survival".to_string());
        let mut sink = EventSink::new(FailThirdFlush::default(), OutputFormat::Text);
        let requests = vec![subagent_request("first"), subagent_request("second")];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("subagent-batch-receipt-survival".to_string());
        let mut conversation = orca_core::conversation::Conversation::new();

        let (batch_result, receipts) =
            super::run_subagent_batch_tool_turn(super::RuntimeSubagentBatchToolTurnContext {
                request: super::RuntimeSubagentBatchToolTurnRequest {
                    config: &config,
                    cwd: cwd.path(),
                    tool_requests: &requests,
                    subagent_depth: 0,
                    child_budgets: Vec::new(),
                    emit_deltas: true,
                },
                io: super::RuntimeSubagentBatchToolTurnIo {
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                },
                services: super::RuntimeSubagentBatchToolTurnServices {
                    instructions: &instructions,
                    memory: &memory,
                    mcp_registry: &mcp_registry,
                    hooks: &hooks,
                },
                runtime: super::RuntimeSubagentBatchToolTurnRuntime {
                    cancel: &cancel,
                    task_registry: &task_registry,
                    root_task_id: None,
                    workflow_ipc: None,
                    activity_ingress: None,
                    permission_handler: None,
                    agent_controller: None,
                },
                child_executor: receipt_child_executor::<std::io::Sink>,
            });
        let error = match batch_result {
            Err(error) => error,
            Ok(_) => panic!("third event flush should fail after recording terminals"),
        };
        assert!(error.to_string().contains("event consumer disconnected"));
        assert_eq!(
            receipts.len(),
            2,
            "the receipt vector survives the persistence failure"
        );
        assert_eq!(
            receipts[0],
            Some(orca_core::budget::BudgetUsage {
                turns: 2,
                tool_calls: 3,
                cost_usd_micros: 700,
                wall_time_ms: 0,
            }),
            "the completed child's consumed usage reaches the parent even when persistence fails"
        );
        assert_eq!(
            receipts[1], None,
            "a child that never started has no receipt"
        );
        assert!(matches!(
            &conversation.messages[0],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Completed
        ));
        assert!(matches!(
            &conversation.messages[1],
            orca_core::conversation::Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == tool_types::ToolStatus::Failed
                && terminal.started == tool_types::ToolInvocationStarted::No
        ));
    }
}
