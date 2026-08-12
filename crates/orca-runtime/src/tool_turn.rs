use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::conversation::Message;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::{McpElicitationHandler, McpRegistry};

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::extension::RuntimeExtensionContext;
use crate::goal_actor::GoalRuntimeBinding;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequestHandler, RuntimeTaskActor,
    RuntimeUserInputHandler,
};
use crate::memory::MemoryBlock;
use crate::operation_context::OperationContext;
#[cfg(test)]
use crate::runtime_readonly_tool_turn::record_readonly_batch_results;
use crate::runtime_readonly_tool_turn::{
    RuntimeReadonlyToolTurnContext, RuntimeReadonlyToolTurnIo, RuntimeReadonlyToolTurnRequest,
    RuntimeReadonlyToolTurnServices, run_readonly_tool_turn,
};
use crate::runtime_surface::{RuntimeProviderResponseIngress, RuntimeWorkflowLifecycleIngress};
use crate::runtime_tool_scheduler::{RuntimeToolDispatch, RuntimeToolDispatchScheduler};
use crate::session::record_tool_result_for_agent;
use crate::step_context::{
    RuntimeSamplingRequestState, RuntimeStepContext, RuntimeToolResultRecordOutcome,
};
use crate::subagent_execution::{
    RuntimeSubagentBatchToolTurnContext, RuntimeSubagentBatchToolTurnIo,
    RuntimeSubagentBatchToolTurnRequest, RuntimeSubagentBatchToolTurnRuntime,
    RuntimeSubagentBatchToolTurnServices, run_subagent_batch_tool_turn,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::{
    ToolExecutionContext, execute_tool_with_approval, is_semantic_commit_failure,
    semantic_commit_failure,
};
use crate::tool_invocation::reject_disallowed_child_tool;
use crate::tool_router::RuntimeToolTurnDisposition;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) enum ToolTurnOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
        /// Typed operation terminal when this turn ended in a budget stop
        /// (already committed to the journal by the operation context).
        terminal: Option<orca_core::budget::OperationTerminal>,
    },
}

pub(crate) struct RuntimeToolTurnsContext<'a, W: io::Write> {
    pub(crate) step_context: RuntimeStepContext<'a>,
    pub(crate) sampling_state: &'a mut RuntimeSamplingRequestState,
    pub(crate) operation: Option<&'a mut OperationContext>,
    pub(crate) io: RuntimeToolTurnsIo<'a, W>,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) executors: RuntimeToolTurnsExecutors,
}

pub(crate) struct RuntimeToolTurnsIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeToolTurnsExecutors {
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct RuntimeNormalToolTurnContext<'a, W: io::Write> {
    pub(crate) sampling_state: &'a mut RuntimeSamplingRequestState,
    pub(crate) request: RuntimeNormalToolTurnRequest<'a>,
    pub(crate) io: RuntimeNormalToolTurnIo<'a, W>,
    pub(crate) services: RuntimeNormalToolTurnServices<'a>,
    pub(crate) runtime: RuntimeNormalToolTurnRuntime<'a>,
    pub(crate) interactions: RuntimeNormalToolTurnInteractions<'a>,
    pub(crate) extensions: Option<RuntimeExtensionContext<'a>>,
    pub(crate) executors: RuntimeNormalToolTurnExecutors,
}

pub(crate) struct RuntimeNormalToolTurnExecution {
    outcome: ToolTurnOutcome,
    event_error: Option<io::Error>,
    /// The subagent child's consumed budget receipt, when this tool ran a
    /// child agent that reported one.
    child_budget_usage: Option<orca_core::budget::BudgetUsage>,
}

pub(crate) struct RuntimeNormalToolTurnRequest<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) tool_request: &'a ToolRequest,
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    pub(crate) goal_mode: bool,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) root_task_id: Option<&'a str>,
    /// Lease-derived budget bound for subagent children of this tool call.
    pub(crate) child_budget: Option<orca_core::budget::BudgetSpec>,
}

pub(crate) struct RuntimeNormalToolTurnIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeNormalToolTurnExecutors {
    pub(crate) subagent_child_executor: ChildAgentExecutor<io::Sink>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
}

pub(crate) struct RuntimeNormalToolTurnServices<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
}

pub(crate) struct RuntimeNormalToolTurnRuntime<'a> {
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) workflow_lifecycle_ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    pub(crate) wait_for_background_workflows: bool,
}

pub(crate) struct RuntimeNormalToolTurnInteractions<'a> {
    pub(crate) approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    pub(crate) mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    pub(crate) provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
}

impl ToolTurnOutcome {
    #[cfg(test)]
    pub(crate) fn from_terminal(status: RunStatus, error: Option<String>) -> Self {
        Self::Return {
            status,
            error,
            terminal: None,
        }
    }

    pub(crate) fn from_record_outcome(outcome: RuntimeToolResultRecordOutcome) -> Self {
        match outcome {
            RuntimeToolResultRecordOutcome::Continue => Self::Continue,
            RuntimeToolResultRecordOutcome::Return { status, error } => Self::Return {
                status,
                error,
                terminal: None,
            },
        }
    }
}

fn subagent_budget_exhaustion_error(
    config: &RunConfig,
    tool_request: &ToolRequest,
    cost_tracker: &CostTracker,
) -> Option<String> {
    if tool_request.name != orca_core::tool_types::ToolName::Subagent {
        return None;
    }
    let max_cost_usd_micros = config.budget.max_cost_usd_micros?;
    let spent_usd_micros = crate::cost::usd_to_micros(cost_tracker.totals().estimated_cost_usd);
    (spent_usd_micros > max_cost_usd_micros).then(|| {
        format!(
            "budget stopped: estimated cost ${:.6} exceeded limit ${:.6}",
            spent_usd_micros as f64 / 1_000_000.0,
            max_cost_usd_micros as f64 / 1_000_000.0
        )
    })
}

#[cfg(test)]
pub(crate) fn terminal_tool_turn(status: RunStatus, error: Option<String>) -> ToolTurnOutcome {
    ToolTurnOutcome::from_terminal(status, error)
}

pub(crate) fn run_tool_turns<W: io::Write>(
    context: RuntimeToolTurnsContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeToolTurnsContext {
        step_context,
        sampling_state,
        mut operation,
        io,
        tool_requests,
        executors,
    } = context;
    let RuntimeToolTurnsExecutors {
        workflow_child_executor,
        batch_child_executor,
    } = executors;
    let RuntimeToolTurnsIo {
        events,
        sink,
        conversation,
        mut history_writer,
        cost_tracker,
        background_workflows,
    } = io;
    let (step_snapshot, extensions) = step_context.into_parts();
    let config = step_snapshot.config;
    let cwd = step_snapshot.turn_context.cwd;
    let tool_policy = step_snapshot.tool_policy;
    let subagent_depth = step_snapshot.turn_context.subagent_depth;
    let root_task_id = step_snapshot.turn_context.root_task_id;
    let emit_deltas = step_snapshot.turn_context.emit_deltas;
    let turn_id = step_snapshot.turn_context.turn_id.as_str();
    let provider_response_ingress = step_snapshot.turn_context.provider_response_ingress();
    let workflow_lifecycle_ingress = step_snapshot.turn_context.workflow_lifecycle_ingress();
    let wait_for_background_workflows = step_snapshot.turn_context.wait_for_background_workflows;
    let policy = step_snapshot.policy;
    let capabilities = step_snapshot.capabilities();
    let instructions = capabilities.instructions;
    let memory = capabilities.memory;
    let mcp_registry = capabilities.mcp_registry;
    let hooks = capabilities.hooks;
    let cancel = capabilities.cancel;
    let task_registry = capabilities.task_registry;
    let workflow_ipc = capabilities.workflow_ipc;
    let approval_handler = capabilities.approval_handler;
    let permission_handler = capabilities.permission_handler;
    let user_input_handler = capabilities.user_input_handler;
    let mcp_elicitation_handler = capabilities.mcp_elicitation_handler;
    while let Some(tool_request) = sampling_state.current_tool_request(tool_requests) {
        if cancel.is_cancelled() {
            close_unstarted_tool_requests(
                sampling_state,
                tool_requests,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                emit_deltas,
                provider_response_ingress,
                "the tool turn was cancelled before dispatch",
            )?;
            return Ok(ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                error: Some("tool turn cancelled".to_string()),
                terminal: None,
            });
        }
        if let Some(result) = reject_disallowed_child_tool(
            tool_request,
            tool_policy,
            mcp_registry,
            &config.external_tools,
        ) {
            let event_error = emit_tool_terminal_events(
                events,
                sink,
                tool_request,
                &result,
                emit_deltas,
                provider_response_ingress,
            )
            .err();
            if let Some(error) = event_error.as_ref()
                && is_semantic_commit_failure(error)
            {
                return Err(io::Error::new(error.kind(), error.to_string()));
            }
            record_tool_result_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                &result,
                emit_deltas,
            )?;
            sampling_state.advance_tool_cursor_one(tool_requests.len());
            let outcome = ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                error: Some(result.error.clone().unwrap_or_default()),
                terminal: None,
            };
            close_unstarted_tool_requests(
                sampling_state,
                tool_requests,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                emit_deltas,
                provider_response_ingress,
                "an earlier sibling was rejected by tool policy",
            )?;
            if let Some(error) = event_error {
                return Err(error);
            }
            return Ok(outcome);
        }

        let Some(dispatch) = RuntimeToolDispatchScheduler::new(config, subagent_depth)
            .next_dispatch(sampling_state, tool_requests)
        else {
            break;
        };

        // Admit every tool in this dispatch window against the operation
        // budget before any of them runs. Already-admitted calls are settled
        // as cancelled and the stop commits a real checkpoint + terminal to
        // the journal before surfacing.
        if let Some(operation) = operation.as_deref_mut() {
            let window_requests: Vec<&ToolRequest> = match &dispatch {
                RuntimeToolDispatch::Normal(request) => vec![*request],
                RuntimeToolDispatch::SubagentBatch(window)
                | RuntimeToolDispatch::ReadonlyBatch(window) => {
                    window.tool_requests().iter().collect()
                }
            };
            let mut admitted_ids: Vec<String> = Vec::new();
            let mut stop = None;
            for request in window_requests {
                match operation.admit_tool_call(turn_id, &request.id, request.name.as_str()) {
                    Ok(Ok(())) => admitted_ids.push(request.id.clone()),
                    Ok(Err(s)) => {
                        stop = Some(s);
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(stop) = stop {
                for id in &admitted_ids {
                    operation.record_tool_completed(
                        turn_id,
                        id,
                        "cancelled",
                        Some("budget stopped before dispatch".to_string()),
                    )?;
                }
                // Settle every unstarted tool first (their conversation
                // results are the resume boundary), then commit the real
                // session boundary + journal checkpoint + terminal.
                close_unstarted_tool_requests(
                    sampling_state,
                    tool_requests,
                    events,
                    sink,
                    conversation,
                    history_writer.as_deref_mut(),
                    emit_deltas,
                    provider_response_ingress,
                    "an earlier tool dispatch was stopped by the operation budget",
                )?;
                let terminal = operation.commit_budget_stop_with_boundary(
                    turn_id,
                    events.run_id(),
                    history_writer.as_deref_mut(),
                    &cost_tracker.totals(),
                    None,
                )?;
                return Ok(ToolTurnOutcome::Return {
                    status: RunStatus::Failed,
                    error: Some(format!(
                        "budget stopped: {} (turns={}, tool_calls={})",
                        stop.reason.as_str(),
                        stop.usage.turns,
                        stop.usage.tool_calls
                    )),
                    terminal: Some(terminal),
                });
            }
        }

        if let RuntimeToolDispatch::SubagentBatch(dispatch_window) = dispatch {
            // Reserve ONE lease per batch child from the parent operation so
            // concurrent children can never double-spend it: each child's
            // effective spec accounts for every earlier child's outstanding
            // reservation. Leases settle back (RAII) with the child receipts.
            let mut child_leases: Vec<Option<crate::budget_controller::BudgetLease>> = Vec::new();
            let mut child_budgets: Vec<Option<orca_core::budget::BudgetSpec>> = Vec::new();
            if let Some(operation) = operation.as_deref_mut() {
                for _ in dispatch_window.tool_requests() {
                    match operation.controller.child_lease(config.budget.to_spec()) {
                        Ok(lease) => {
                            child_budgets.push(Some(*lease.spec()));
                            child_leases.push(Some(lease));
                        }
                        Err(stop) => {
                            // The parent has no capacity left for another
                            // child: settle the already-granted leases and
                            // stop the batch through the typed budget stop.
                            for lease in child_leases.drain(..).flatten() {
                                let consumed = lease.finish();
                                let _ = operation.controller.merge_child_usage(consumed);
                            }
                            let terminal = operation.commit_budget_stop_with_boundary(
                                turn_id,
                                events.run_id(),
                                history_writer.as_deref_mut(),
                                &cost_tracker.totals(),
                                None,
                            )?;
                            close_unstarted_tool_requests(
                                sampling_state,
                                tool_requests,
                                events,
                                sink,
                                conversation,
                                history_writer.as_deref_mut(),
                                emit_deltas,
                                provider_response_ingress,
                                "an earlier subagent dispatch was stopped by the operation budget",
                            )?;
                            return Ok(ToolTurnOutcome::Return {
                                status: RunStatus::Failed,
                                error: Some(format!(
                                    "budget stopped: {} (turns={}, tool_calls={})",
                                    stop.reason.as_str(),
                                    stop.usage.turns,
                                    stop.usage.tool_calls
                                )),
                                terminal: Some(terminal),
                            });
                        }
                    }
                }
            }
            let batch_result = run_subagent_batch_tool_turn(RuntimeSubagentBatchToolTurnContext {
                request: RuntimeSubagentBatchToolTurnRequest {
                    config,
                    cwd,
                    tool_requests: dispatch_window.tool_requests(),
                    subagent_depth,
                    emit_deltas,
                    child_budgets,
                },
                io: RuntimeSubagentBatchToolTurnIo {
                    events,
                    sink,
                    conversation,
                    history_writer: history_writer.as_deref_mut(),
                    cost_tracker,
                },
                services: RuntimeSubagentBatchToolTurnServices {
                    instructions,
                    memory,
                    mcp_registry,
                    hooks,
                },
                runtime: RuntimeSubagentBatchToolTurnRuntime {
                    cancel,
                    task_registry,
                    root_task_id,
                    workflow_ipc,
                },
                child_executor: batch_child_executor,
            });
            // The cursor advances past the whole window before error
            // handling, so the unstarted-close below never double-records
            // children the batch already settled.
            sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
            let (outcome, batch_receipts) = match batch_result {
                Ok(pair) => pair,
                Err(error) => {
                    // The batch failed before producing receipts; every
                    // granted lease still settles (reservation returns).
                    for lease in child_leases.drain(..).flatten() {
                        let consumed = lease.finish();
                        if let Some(operation) = operation.as_deref_mut() {
                            let _ = operation.controller.merge_child_usage(consumed);
                        }
                    }
                    close_unstarted_tool_requests(
                        sampling_state,
                        tool_requests,
                        events,
                        sink,
                        conversation,
                        history_writer.as_deref_mut(),
                        emit_deltas,
                        provider_response_ingress,
                        "an earlier subagent batch ended after an event I/O error",
                    )?;
                    return Err(error);
                }
            };
            if let Some(operation) = operation.as_deref_mut() {
                for request in dispatch_window.tool_requests() {
                    operation.record_tool_completed(
                        turn_id,
                        &request.id,
                        conversation_tool_settlement(conversation, &request.id).0,
                        conversation_tool_settlement(conversation, &request.id).1,
                    )?;
                }
            }
            if matches!(outcome, ToolTurnOutcome::Return { .. }) {
                for (lease, receipt) in child_leases
                    .drain(..)
                    .flatten()
                    .zip(batch_receipts.into_iter())
                {
                    settle_child_lease(&mut Some(lease), receipt, &mut operation);
                }
                close_unstarted_tool_requests(
                    sampling_state,
                    tool_requests,
                    events,
                    sink,
                    conversation,
                    history_writer.as_deref_mut(),
                    emit_deltas,
                    provider_response_ingress,
                    "an earlier sibling ended the tool turn",
                )?;
                return Ok(outcome);
            }
            for (lease, receipt) in child_leases
                .drain(..)
                .flatten()
                .zip(batch_receipts.into_iter())
            {
                settle_child_lease(&mut Some(lease), receipt, &mut operation);
            }
            continue;
        }

        if let RuntimeToolDispatch::ReadonlyBatch(dispatch_window) = dispatch {
            let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
                request: RuntimeReadonlyToolTurnRequest {
                    cwd,
                    tool_requests: dispatch_window.tool_requests(),
                    emit_deltas,
                    cancel,
                    output_truncation: config.tools.output_truncation,
                    max_parallel: config.tools.max_read_parallel,
                },
                io: RuntimeReadonlyToolTurnIo {
                    events,
                    sink,
                    conversation,
                    history_writer: history_writer.as_deref_mut(),
                },
                services: RuntimeReadonlyToolTurnServices {
                    mcp_registry,
                    hooks,
                    provider_response_ingress,
                    task_registry: Some(task_registry),
                    root_task_id,
                },
            });
            let outcome = match outcome {
                Ok(outcome) => {
                    sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
                    outcome
                }
                Err(error) if is_semantic_commit_failure(&error) => return Err(error),
                Err(error) => {
                    sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
                    close_unstarted_tool_requests(
                        sampling_state,
                        tool_requests,
                        events,
                        sink,
                        conversation,
                        history_writer.as_deref_mut(),
                        emit_deltas,
                        provider_response_ingress,
                        "an earlier read-only batch completed before an event I/O error",
                    )?;
                    return Err(error);
                }
            };
            if let Some(operation) = operation.as_deref_mut() {
                for request in dispatch_window.tool_requests() {
                    operation.record_tool_completed(
                        turn_id,
                        &request.id,
                        conversation_tool_settlement(conversation, &request.id).0,
                        conversation_tool_settlement(conversation, &request.id).1,
                    )?;
                }
            }
            if matches!(outcome, ToolTurnOutcome::Return { .. }) {
                close_unstarted_tool_requests(
                    sampling_state,
                    tool_requests,
                    events,
                    sink,
                    conversation,
                    history_writer.as_deref_mut(),
                    emit_deltas,
                    provider_response_ingress,
                    "an earlier sibling ended the tool turn",
                )?;
                return Ok(outcome);
            }
            continue;
        }

        let RuntimeToolDispatch::Normal(tool_request) = dispatch else {
            unreachable!("batch dispatches are handled before normal tool dispatch");
        };

        // Reserve the child budget from the parent operation for subagent
        // children of this tool call; the effective spec bounds the child and
        // settles back (RAII) once the tool settles.
        let mut child_lease = if tool_request.name == orca_core::tool_types::ToolName::Subagent {
            operation.as_deref_mut().and_then(|operation| {
                operation
                    .controller
                    .child_lease(config.budget.to_spec())
                    .ok()
            })
        } else {
            None
        };

        let execution = run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state,
            request: RuntimeNormalToolTurnRequest {
                config,
                cwd,
                tool_request,
                subagent_depth,
                emit_deltas,
                goal_mode: tool_policy.is_goal_mode(),
                policy,
                root_task_id,
                child_budget: child_lease.as_ref().map(|lease| *lease.spec()),
            },
            io: RuntimeNormalToolTurnIo {
                events,
                sink,
                conversation,
                history_writer: history_writer.as_deref_mut(),
                cost_tracker,
                background_workflows,
            },
            services: RuntimeNormalToolTurnServices {
                instructions,
                memory,
                mcp_registry,
                hooks,
            },
            runtime: RuntimeNormalToolTurnRuntime {
                cancel,
                task_registry,
                workflow_ipc,
                workflow_lifecycle_ingress,
                wait_for_background_workflows,
            },
            interactions: RuntimeNormalToolTurnInteractions {
                approval_handler,
                permission_handler,
                user_input_handler,
                mcp_elicitation_handler,
                provider_response_ingress,
            },
            extensions,
            executors: RuntimeNormalToolTurnExecutors {
                subagent_child_executor: batch_child_executor,
                workflow_child_executor,
            },
        });
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) => {
                settle_child_lease(&mut child_lease, None, &mut operation);
                if is_semantic_commit_failure(&error) {
                    return Err(error);
                }
                if conversation_has_tool_result(conversation, &tool_request.id) {
                    sampling_state.advance_tool_cursor_one(tool_requests.len());
                    close_unstarted_tool_requests(
                        sampling_state,
                        tool_requests,
                        events,
                        sink,
                        conversation,
                        history_writer.as_deref_mut(),
                        emit_deltas,
                        provider_response_ingress,
                        "an earlier tool result was recorded before persistence failed",
                    )?;
                    return Err(error);
                }
                let result = ToolResult::indeterminate(
                    tool_request,
                    format!(
                        "Tool invocation outcome is indeterminate after runtime I/O error: {error}. Inspect external state before retrying."
                    ),
                );
                emit_tool_terminal_events(
                    events,
                    sink,
                    tool_request,
                    &result,
                    emit_deltas,
                    provider_response_ingress,
                )?;
                record_tool_result_for_agent(
                    conversation,
                    history_writer.as_deref_mut(),
                    &result,
                    emit_deltas,
                )?;
                if let Some(operation) = operation.as_deref_mut() {
                    operation.record_tool_completed(
                        turn_id,
                        &tool_request.id,
                        "indeterminate",
                        Some(result.error.clone().unwrap_or_default()),
                    )?;
                }
                sampling_state.advance_tool_cursor_one(tool_requests.len());
                close_unstarted_tool_requests(
                    sampling_state,
                    tool_requests,
                    events,
                    sink,
                    conversation,
                    history_writer.as_deref_mut(),
                    emit_deltas,
                    provider_response_ingress,
                    "an earlier tool invocation ended after a runtime I/O error",
                )?;
                return Err(error);
            }
        };
        sampling_state.advance_tool_cursor_one(tool_requests.len());
        if let Some(error) = execution.event_error {
            settle_child_lease(
                &mut child_lease,
                execution.child_budget_usage,
                &mut operation,
            );
            close_unstarted_tool_requests(
                sampling_state,
                tool_requests,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                emit_deltas,
                provider_response_ingress,
                "an earlier tool invocation completed before an event I/O error",
            )?;
            return Err(error);
        }
        let outcome = execution.outcome;
        if let Some(operation) = operation.as_deref_mut() {
            operation.record_tool_completed(
                turn_id,
                &tool_request.id,
                conversation_tool_settlement(conversation, &tool_request.id).0,
                conversation_tool_settlement(conversation, &tool_request.id).1,
            )?;
        }
        if matches!(outcome, ToolTurnOutcome::Return { .. }) {
            settle_child_lease(
                &mut child_lease,
                execution.child_budget_usage,
                &mut operation,
            );
            close_unstarted_tool_requests(
                sampling_state,
                tool_requests,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                emit_deltas,
                provider_response_ingress,
                "an earlier sibling ended the tool turn",
            )?;
            return Ok(outcome);
        }
        settle_child_lease(
            &mut child_lease,
            execution.child_budget_usage,
            &mut operation,
        );
    }

    Ok(ToolTurnOutcome::Continue)
}

/// Settles a child budget lease exactly once: the child's consumed usage
/// receipt (when the child reported one) merges into the parent operation and
/// the reservation returns to its pool. Safe to call repeatedly on every
/// branch exit; only the first call settles.
fn settle_child_lease(
    child_lease: &mut Option<crate::budget_controller::BudgetLease>,
    child_usage: Option<orca_core::budget::BudgetUsage>,
    operation: &mut Option<&mut OperationContext>,
) {
    let Some(lease) = child_lease.take() else {
        return;
    };
    let consumed = child_usage.unwrap_or_else(|| lease.finish());
    if let Some(operation) = operation.as_deref_mut() {
        // Merging past the parent's ceiling latches the parent stop; the
        // next turn admission surfaces it as a typed terminal.
        let _ = operation.controller.merge_child_usage(consumed);
    }
}

/// The final settlement label for a tool from the committed conversation
/// (the same source the tool events and recovery read), so the journal never
/// disagrees with the conversation about a tool's outcome.
fn conversation_tool_settlement(
    conversation: &Conversation,
    tool_call_id: &str,
) -> (&'static str, Option<String>) {
    conversation
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Tool {
                tool_call_id: id,
                terminal: Some(terminal),
                ..
            } if id == tool_call_id => Some((tool_status_label(&terminal.status), None)),
            _ => None,
        })
        .unwrap_or(("completed", None))
}

fn tool_status_label(status: &orca_core::tool_types::ToolStatus) -> &'static str {
    use orca_core::tool_types::ToolStatus;
    match status {
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Indeterminate => "indeterminate",
        ToolStatus::Denied => "denied",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::NotImplemented => "not_implemented",
    }
}

fn close_unstarted_tool_requests<W: io::Write>(
    sampling_state: &mut RuntimeSamplingRequestState,
    tool_requests: &[ToolRequest],
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    emit_deltas: bool,
    provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
    reason: &str,
) -> io::Result<()> {
    let pending_window =
        sampling_state.tool_dispatch_window(tool_requests, |requests, _| requests.len());
    let pending = pending_window.tool_requests();
    let results = pending
        .iter()
        .map(|tool_request| ToolResult::cancelled_before_start(tool_request, reason))
        .collect::<Vec<_>>();
    if results.is_empty() {
        return Ok(());
    }
    if let Some(ingress) = provider_response_ingress {
        ingress
            .commit_tool_results(&results)
            .map_err(semantic_commit_failure)?;
    }
    let mut first_error = None;
    for (tool_request, result) in pending.iter().zip(results.iter()) {
        if let Err(error) =
            emit_tool_terminal_events(events, sink, tool_request, result, emit_deltas, None)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            result,
            emit_deltas,
        ) && first_error.is_none()
        {
            first_error = Some(error);
        }
        sampling_state.advance_tool_cursor_one(tool_requests.len());
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn conversation_has_tool_result(conversation: &Conversation, tool_call_id: &str) -> bool {
    conversation.messages.iter().rev().any(|message| {
        matches!(
            message,
            orca_core::conversation::Message::Tool {
                tool_call_id: recorded_id,
                ..
            } if recorded_id == tool_call_id
        )
    })
}

fn emit_tool_terminal_events<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    request: &ToolRequest,
    result: &ToolResult,
    emit_deltas: bool,
    provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
) -> io::Result<()> {
    if let Some(ingress) = provider_response_ingress {
        ingress
            .commit_tool_result(result)
            .map_err(semantic_commit_failure)?;
    }
    if !emit_deltas {
        return Ok(());
    }
    let requested = sink.emit(RuntimeTaskActor::tool_call_requested_event_for(
        events, request,
    ));
    let completed = sink.emit(RuntimeTaskActor::tool_call_completed_event_for(
        events, request, result,
    ));
    requested.and(completed)
}

pub(crate) fn run_normal_tool_turn<W: io::Write>(
    context: RuntimeNormalToolTurnContext<'_, W>,
) -> io::Result<RuntimeNormalToolTurnExecution> {
    let RuntimeNormalToolTurnContext {
        sampling_state,
        request,
        io,
        services,
        runtime,
        interactions,
        extensions,
        executors,
    } = context;
    let RuntimeNormalToolTurnRequest {
        config,
        cwd,
        tool_request,
        subagent_depth,
        emit_deltas,
        goal_mode,
        policy,
        root_task_id,
        child_budget,
    } = request;
    let RuntimeNormalToolTurnExecutors {
        subagent_child_executor,
        workflow_child_executor,
    } = executors;
    let RuntimeNormalToolTurnIo {
        events,
        sink,
        conversation,
        mut history_writer,
        cost_tracker,
        background_workflows,
    } = io;
    let RuntimeNormalToolTurnServices {
        instructions,
        memory,
        mcp_registry,
        hooks,
    } = services;
    let RuntimeNormalToolTurnRuntime {
        cancel,
        task_registry,
        workflow_ipc,
        workflow_lifecycle_ingress,
        wait_for_background_workflows,
    } = runtime;
    let RuntimeNormalToolTurnInteractions {
        approval_handler,
        permission_handler,
        user_input_handler,
        mcp_elicitation_handler,
        provider_response_ingress,
    } = interactions;
    if let Some(error) = subagent_budget_exhaustion_error(config, tool_request, cost_tracker) {
        let result = ToolResult::failed_before_start(tool_request, error.clone(), None);
        let event_error = emit_tool_terminal_events(
            events,
            sink,
            tool_request,
            &result,
            emit_deltas,
            provider_response_ingress,
        )
        .err();
        sampling_state.record_normal_tool_result(
            conversation,
            history_writer.as_deref_mut(),
            tool_request,
            &result,
            RunStatus::Failed,
            emit_deltas,
        )?;
        return Ok(RuntimeNormalToolTurnExecution {
            outcome: ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                error: Some(error),
                terminal: None,
            },
            event_error,
            child_budget_usage: None,
        });
    }
    let mut execution_context = ToolExecutionContext::new(cwd, subagent_depth, emit_deltas, policy)
        .with_goal_mode(goal_mode)
        .with_root_task_id(root_task_id)
        .with_child_budget(child_budget)
        .with_services(instructions, memory, mcp_registry, hooks)
        .with_runtime(
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
        )
        .with_workflow_lifecycle(workflow_lifecycle_ingress, wait_for_background_workflows)
        .with_permission_overlay(sampling_state.permission_overlay_mut())
        .with_approval_handler(approval_handler)
        .with_permission_handler(permission_handler)
        .with_user_input_handler(user_input_handler)
        .with_mcp_elicitation_handler(mcp_elicitation_handler)
        .with_provider_response_ingress(provider_response_ingress);
    if let Some(extensions) = extensions {
        execution_context =
            execution_context.with_extensions(extensions.registry(), extensions.stores());
        if let Some(binding) = extensions
            .stores()
            .thread_store()
            .get::<GoalRuntimeBinding>()
        {
            execution_context = execution_context.with_goal_runtime_binding((*binding).clone());
        }
    }
    let mut execution = execute_tool_with_approval(
        config,
        events,
        sink,
        tool_request,
        execution_context,
        subagent_child_executor,
        workflow_child_executor,
    )?;
    if execution
        .event_error
        .as_ref()
        .is_some_and(is_semantic_commit_failure)
    {
        return Err(execution
            .event_error
            .take()
            .expect("semantic commit failure remains available"));
    }

    let budget_exhaustion = subagent_budget_exhaustion_error(config, tool_request, cost_tracker);
    let mut event_error = execution.event_error;
    if budget_exhaustion.is_some() {
        sampling_state.record_normal_tool_result(
            conversation,
            history_writer.as_deref_mut(),
            tool_request,
            &execution.result,
            execution.status,
            emit_deltas,
        )?;
        let totals = cost_tracker.totals();
        if emit_deltas {
            if let Err(error) = sink.emit(events.usage_updated(totals))
                && event_error.is_none()
            {
                event_error = Some(error);
            }
            if let Some(writer) = history_writer.as_deref_mut()
                && let Err(error) = writer.append_usage(totals)
                && event_error.is_none()
            {
                event_error = Some(error);
            }
        }
    }

    let outcome = if let Some(error) = budget_exhaustion {
        ToolTurnOutcome::Return {
            status: RunStatus::Failed,
            error: Some(error),
            terminal: None,
        }
    } else {
        let record_outcome = sampling_state.record_normal_tool_result(
            conversation,
            history_writer.as_deref_mut(),
            tool_request,
            &execution.result,
            execution.status,
            emit_deltas,
        )?;
        if execution.disposition == RuntimeToolTurnDisposition::StopTurn {
            ToolTurnOutcome::Return {
                status: execution.status,
                error: execution.result.error.clone(),
                terminal: None,
            }
        } else {
            ToolTurnOutcome::from_record_outcome(record_outcome)
        }
    };
    record_root_task_output_truncation(
        task_registry,
        root_task_id,
        execution.result.truncated,
        events,
        sink,
    )?;
    Ok(RuntimeNormalToolTurnExecution {
        outcome,
        event_error,
        child_budget_usage: execution.child_budget_usage,
    })
}

pub(crate) fn record_root_task_output_truncation<W: io::Write>(
    task_registry: &TaskRegistry,
    root_task_id: Option<&str>,
    truncated: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<()> {
    if !truncated {
        return Ok(());
    }
    let Some(task_id) = root_task_id else {
        return Ok(());
    };
    task_registry
        .mark_output_truncated(task_id)
        .map_err(io::Error::other)?;
    let task = task_registry.summary(task_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("root task '{task_id}' disappeared after recording output truncation"),
        )
    })?;
    sink.emit(events.task_status_updated(&task))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::time::{Duration, Instant};

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::{Conversation, Message};
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::{HookConfig, HookEvent};
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::Usage;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{
        ToolInvocationStarted, ToolName, ToolRequest, ToolResult, ToolStatus,
    };
    use serde_json::json;

    use super::*;
    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::hooks::HookRunner;
    use crate::tool_execution::policy_for_tool_execution;
    use crate::tool_invocation::AgentToolPolicyContext;

    #[derive(Debug)]
    struct FailingToolResultIngress;

    impl RuntimeProviderResponseIngress for FailingToolResultIngress {
        fn commit_response(
            &self,
            _response: &crate::model_response::RuntimeModelResponse,
        ) -> io::Result<()> {
            Ok(())
        }

        fn commit_provider_step(
            &self,
            _identity: &orca_core::thread_item_projection::ModelResponseIdentity,
            _step: &orca_core::provider_types::ProviderStep,
        ) -> io::Result<()> {
            Ok(())
        }

        fn commit_tool_results(&self, _results: &[ToolResult]) -> io::Result<()> {
            Err(io::Error::other("semantic ledger unavailable"))
        }
    }

    #[test]
    fn semantic_commit_failure_precedes_closed_sibling_history_recording() {
        let request = ToolRequest {
            id: "pending-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf pending".to_string()),
            raw_arguments: Some(r#"{"command":"printf pending"}"#.to_string()),
        };
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let mut events = EventFactory::new("semantic-before-history".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();

        let error = close_unstarted_tool_requests(
            &mut sampling_state,
            std::slice::from_ref(&request),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            true,
            Some(&FailingToolResultIngress),
            "test terminalization",
        )
        .expect_err("semantic failure must stop before history recording");

        assert!(is_semantic_commit_failure(&error));
        assert_eq!(sampling_state.tool_cursor_position(), 0);
        assert!(!conversation_has_tool_result(&conversation, &request.id));
    }

    #[test]
    fn closing_zero_siblings_skips_semantic_commit() {
        let request = ToolRequest {
            id: "already-finished".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf done".to_string()),
            raw_arguments: Some(r#"{"command":"printf done"}"#.to_string()),
        };
        let mut sampling_state = RuntimeSamplingRequestState::new();
        sampling_state.advance_tool_cursor_one(1);
        let mut events = EventFactory::new("zero-sibling-close".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();

        close_unstarted_tool_requests(
            &mut sampling_state,
            std::slice::from_ref(&request),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            true,
            Some(&FailingToolResultIngress),
            "nothing remains",
        )
        .expect("an empty sibling set must not reach semantic ingress");

        assert_eq!(sampling_state.tool_cursor_position(), 1);
        assert!(conversation.messages.is_empty());
    }

    #[test]
    fn truncated_tool_output_marks_root_task_and_emits_visible_summary() {
        let registry = TaskRegistry::new("truncation-root".to_string());
        let task = registry.create_main_session("read a large file".to_string());
        registry.mark_running(&task.id).expect("root task running");
        let mut events = EventFactory::new("truncation-root".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        record_root_task_output_truncation(&registry, Some(&task.id), true, &mut events, &mut sink)
            .expect("record truncation");

        let summary = registry.summary(&task.id).expect("root task summary");
        assert!(summary.output_truncated);
        assert!(
            String::from_utf8(output)
                .expect("event output")
                .contains("outputTruncated")
        );
    }

    fn config_with_external(external_tools: Vec<ExternalToolConfig>) -> RunConfig {
        RunConfig {
            prompt: "test".to_string(),
            app_version: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            provider: ProviderKind::Mock,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            approval_mode: ApprovalMode::Suggest,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                ..SubagentConfig::default()
            },
            tools: ToolConfig::default(),
            external_tools,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn request(
        name: ToolName,
        action: ActionKind,
        target: Option<&str>,
        raw: Option<&str>,
    ) -> ToolRequest {
        ToolRequest {
            id: "tool-1".to_string(),
            name,
            action,
            target: target.map(str::to_string),
            raw_arguments: raw.map(str::to_string),
        }
    }

    fn history_writer_that_fails_on_append(label: &str) -> (tempfile::TempDir, SessionWriter) {
        let history = tempfile::tempdir().expect("history tempdir");
        let history_path = history.path().join("session.jsonl");
        let meta = crate::history::create_meta(history.path(), "mock", None, label);
        let mut meta_record = serde_json::to_value(meta)
            .expect("serialize history metadata")
            .as_object()
            .cloned()
            .expect("history metadata object");
        meta_record.insert("type".to_string(), json!("session.meta"));
        std::fs::write(
            &history_path,
            format!("{}\n", serde_json::Value::Object(meta_record)),
        )
        .expect("seed history file");
        let mut writer =
            SessionWriter::append_to_existing(history_path.clone()).expect("open existing history");
        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        std::fs::remove_file(&history_path).expect("remove history file");
        std::fs::create_dir(&history_path).expect("replace history file with directory");
        (history, writer)
    }

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("read_file turn must not execute child agents")
    }

    fn budget_crossing_child_executor<W: io::Write>(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        assert_eq!(request.prompt, "first child");
        child_cost_tracker.add_usage(Usage {
            input_tokens: 10_000_000,
            output_tokens: 0,
            cache_tokens: 0,
        });
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("first child completed".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn delayed_batch_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        std::thread::sleep(Duration::from_millis(100));
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("child finished".to_string()),
            error: None,
            budget_usage: None,
        })
    }

    fn scoped_cancellation_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        let root_task_id = runtime
            .root_task_id
            .expect("subagent execution must preserve the parent root task id");
        let registry = runtime
            .task_registry
            .expect("subagent execution must preserve the task registry");
        let descendant = registry.create_subagent_with_parent(
            "root-scoped descendant".to_string(),
            Some("general".to_string()),
            Some(root_task_id.to_string()),
        );
        registry
            .mark_running(&descendant.id)
            .expect("start root-scoped descendant");
        let unrelated = registry
            .list()
            .into_iter()
            .find(|task| task.description == "unrelated branch")
            .expect("unrelated task fixture");

        let stopped = registry
            .signal_stop_tree(root_task_id)
            .expect("stop the selected task tree");

        assert!(stopped.iter().any(|task_id| task_id == root_task_id));
        assert!(stopped.contains(&descendant.id));
        assert!(!stopped.contains(&unrelated.id));
        assert_eq!(
            registry.get(&unrelated.id).expect("unrelated task").status,
            orca_core::task_types::TaskStatus::Running
        );
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("root-scoped cancellation".to_string()),
            budget_usage: None,
        })
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

    #[derive(Default)]
    struct FailSecondFlush {
        flushes: usize,
    }

    impl io::Write for FailSecondFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 2 {
                return Err(io::Error::other("tool event consumer disconnected"));
            }
            Ok(())
        }
    }

    #[test]
    fn sampling_request_state_advances_over_single_and_batch_tool_requests() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let third = ToolRequest {
            id: "tool-3".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let requests = vec![first, second, third];

        let mut sampling_state = RuntimeSamplingRequestState::new();

        assert_eq!(
            sampling_state
                .current_tool_request(&requests)
                .map(|request| request.id.as_str()),
            Some("tool-1")
        );
        sampling_state.advance_tool_cursor_one(requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 1);
        assert_eq!(
            sampling_state
                .current_tool_request(&requests)
                .map(|request| request.id.as_str()),
            Some("tool-2")
        );
        sampling_state.advance_tool_cursor_to(3, requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert!(sampling_state.current_tool_request(&requests).is_none());
        sampling_state.advance_tool_cursor_to(99, requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 3);
    }

    #[test]
    fn sampling_request_state_builds_and_advances_dispatch_windows() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let third = ToolRequest {
            id: "tool-3".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let requests = vec![first, second, third];
        let mut sampling_state = RuntimeSamplingRequestState::new();

        sampling_state.advance_tool_cursor_one(requests.len());
        let dispatch_window = sampling_state
            .tool_dispatch_window(&requests, |_, start_index| start_index.saturating_add(99));

        assert_eq!(
            dispatch_window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-2", "tool-3"]
        );
        assert_eq!(dispatch_window.end_index(), 3);
        sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert!(sampling_state.current_tool_request(&requests).is_none());

        let mut sampling_state = RuntimeSamplingRequestState::new();
        sampling_state.advance_tool_cursor_one(requests.len());
        let stalled_window =
            sampling_state.tool_dispatch_window(&requests, |_, start_index| start_index);

        assert_eq!(
            stalled_window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-2"]
        );
        assert_eq!(stalled_window.end_index(), 2);
    }

    #[test]
    fn sampling_request_state_records_approval_required_normal_tool_result() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(
            ToolName::RequestPermissions,
            ActionKind::Read,
            Some("read"),
            None,
        );
        let result = ToolResult::denied(&request, "needs approval");

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::ApprovalRequired,
                false,
            )
            .expect("record approval result");

        match outcome {
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::ApprovalRequired);
                assert_eq!(error.as_deref(), Some("needs approval"));
            }
            RuntimeToolResultRecordOutcome::Continue => {
                panic!("approval-required result must return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn sampling_request_state_records_subagent_failure_normal_tool_result() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);
        let result = ToolResult::failed(&request, "child failed", None);

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::Failed,
                false,
            )
            .expect("record subagent failure");

        match outcome {
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            RuntimeToolResultRecordOutcome::Continue => {
                panic!("failed subagent result must return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn sampling_request_state_records_cancelled_normal_tool_result_as_terminal() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(
            ToolName::AskUserQuestion,
            ActionKind::Read,
            Some("Continue?"),
            Some(
                r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#,
            ),
        );
        let result = ToolResult::cancelled(&request, "user input cancelled", None);

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::Cancelled,
                false,
            )
            .expect("record cancelled result");

        match outcome {
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Cancelled);
                assert_eq!(error.as_deref(), Some("user input cancelled"));
            }
            RuntimeToolResultRecordOutcome::Continue => {
                panic!("cancelled result must end the tool turn")
            }
        }
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if tool_call_id == "tool-1"
                && terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::Yes
        ));
    }

    #[test]
    fn sampling_request_state_returns_for_indeterminate_normal_tool_result() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(
            ToolName::AskUserQuestion,
            ActionKind::Read,
            Some("Continue?"),
            Some(
                r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#,
            ),
        );
        let result = ToolResult::indeterminate(&request, "interaction channel closed");

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::Failed,
                false,
            )
            .expect("record indeterminate result");

        assert!(matches!(
            outcome,
            RuntimeToolResultRecordOutcome::Return {
                status: RunStatus::Failed,
                error: Some(ref error),
            } if error == "interaction channel closed"
        ));
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Indeterminate
        ));
    }

    #[test]
    fn record_readonly_batch_results_records_each_tool_message_in_order() {
        let mut conversation = Conversation::new();
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let results = vec![
            ToolResult::completed(&first, "one".to_string(), false),
            ToolResult::completed(&second, "two".to_string(), false),
        ];

        record_readonly_batch_results(&mut conversation, None, results, false)
            .expect("record readonly batch results");

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
        assert!(
            matches!(&conversation.messages[1], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-2")
        );
    }

    #[test]
    fn record_readonly_batch_results_keeps_live_terminals_after_history_failure() {
        let (_history, mut writer) =
            history_writer_that_fails_on_append("readonly batch history failure");
        let mut conversation = Conversation::new();
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("two.txt".to_string()),
            raw_arguments: None,
        };

        let error = record_readonly_batch_results(
            &mut conversation,
            Some(&mut writer),
            vec![
                ToolResult::completed(&first, "one".to_string(), false),
                ToolResult::completed(&second, "two".to_string(), false),
            ],
            true,
        )
        .expect_err("history append must fail");

        assert!(error.raw_os_error().is_some());
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    Message::Tool { tool_call_id, .. } => tool_call_id.as_str(),
                    _ => panic!("expected tool result"),
                })
                .collect::<Vec<_>>(),
            ["tool-1", "tool-2"]
        );
    }

    #[test]
    fn run_normal_tool_turn_executes_and_records_tool_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        std::fs::write(cwd.path().join("other.txt"), "world\n").expect("write file");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("normal-tool-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::ReadFile,
            ActionKind::Read,
            Some("tracked.txt"),
            Some(json!({ "path": "tracked.txt" }).to_string().as_str()),
        );
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("normal-tool-turn".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();

        let outcome = run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state: &mut sampling_state,
            request: RuntimeNormalToolTurnRequest {
                config: &config,
                cwd: cwd.path(),
                tool_request: &request,
                subagent_depth: 0,
                emit_deltas: false,
                goal_mode: false,
                policy: &policy,
                root_task_id: None,
                child_budget: None,
            },
            io: RuntimeNormalToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            services: RuntimeNormalToolTurnServices {
                instructions: &instructions,
                memory: &memory,
                mcp_registry: &registry,
                hooks: &hooks,
            },
            runtime: RuntimeNormalToolTurnRuntime {
                cancel: &cancel,
                task_registry: &task_registry,
                workflow_ipc: None,
                workflow_lifecycle_ingress: None,
                wait_for_background_workflows: true,
            },
            interactions: RuntimeNormalToolTurnInteractions {
                approval_handler: None,
                permission_handler: None,
                user_input_handler: None,
                mcp_elicitation_handler: None,
                provider_response_ingress: None,
            },
            extensions: None,
            executors: RuntimeNormalToolTurnExecutors {
                subagent_child_executor: unused_child_executor::<io::Sink>,
                workflow_child_executor: unused_child_executor,
            },
        })
        .expect("run normal tool turn");

        assert!(matches!(outcome.outcome, ToolTurnOutcome::Continue));
        assert!(outcome.event_error.is_none());
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "tool-1" && content.contains("hello"))
        );
    }

    #[test]
    fn run_normal_tool_turn_preserves_root_task_cancellation_scope() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let request = ToolRequest {
            id: "scoped-child".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("inspect cancellation scope".to_string()),
            raw_arguments: Some(
                json!({
                    "description": "inspect cancellation scope",
                    "prompt": "inspect cancellation scope",
                    "mode": "sync"
                })
                .to_string(),
            ),
        };
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("root-task-scope".to_string());
        let root = task_registry.create_main_session("selected root".to_string());
        task_registry
            .mark_running(&root.id)
            .expect("start root task");
        let unrelated = task_registry.create_main_session("unrelated branch".to_string());
        task_registry
            .mark_running(&unrelated.id)
            .expect("start unrelated task");
        let mut events = EventFactory::new("root-task-scope".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();

        let execution = run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state: &mut sampling_state,
            request: RuntimeNormalToolTurnRequest {
                config: &config,
                cwd: cwd.path(),
                tool_request: &request,
                subagent_depth: 0,
                emit_deltas: false,
                goal_mode: false,
                policy: &policy,
                root_task_id: Some(&root.id),
                child_budget: None,
            },
            io: RuntimeNormalToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            services: RuntimeNormalToolTurnServices {
                instructions: &instructions,
                memory: &memory,
                mcp_registry: &registry,
                hooks: &hooks,
            },
            runtime: RuntimeNormalToolTurnRuntime {
                cancel: &cancel,
                task_registry: &task_registry,
                workflow_ipc: None,
                workflow_lifecycle_ingress: None,
                wait_for_background_workflows: true,
            },
            interactions: RuntimeNormalToolTurnInteractions {
                approval_handler: None,
                permission_handler: None,
                user_input_handler: None,
                mcp_elicitation_handler: None,
                provider_response_ingress: None,
            },
            extensions: None,
            executors: RuntimeNormalToolTurnExecutors {
                subagent_child_executor: scoped_cancellation_child_executor::<io::Sink>,
                workflow_child_executor: unused_child_executor,
            },
        })
        .expect("run scoped subagent turn");

        assert!(matches!(
            execution.outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert_eq!(
            task_registry.get(&root.id).expect("root task").status,
            orca_core::task_types::TaskStatus::Stopping
        );
        assert_eq!(
            task_registry
                .get(&unrelated.id)
                .expect("unrelated task")
                .status,
            orca_core::task_types::TaskStatus::Running
        );
    }

    #[test]
    fn run_readonly_tool_turn_executes_and_records_batch_results() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        let mut events = EventFactory::new("readonly-tool-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let requests = vec![
            request(
                ToolName::ReadFile,
                ActionKind::Read,
                Some("tracked.txt"),
                Some(json!({ "path": "tracked.txt" }).to_string().as_str()),
            ),
            ToolRequest {
                id: "tool-2".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("other.txt".to_string()),
                raw_arguments: Some(json!({ "path": "other.txt" }).to_string()),
            },
        ];
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
            request: RuntimeReadonlyToolTurnRequest {
                cwd: cwd.path(),
                tool_requests: &requests,
                emit_deltas: false,
                cancel: &CancelToken::new(),
                output_truncation: ToolConfig::default().output_truncation,
                max_parallel: 2,
            },
            io: RuntimeReadonlyToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
            },
            services: RuntimeReadonlyToolTurnServices {
                mcp_registry: &registry,
                hooks: &hooks,
                provider_response_ingress: None,
                task_registry: None,
                root_task_id: None,
            },
        })
        .expect("run readonly tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 2);
        let combined_tool_content = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. }
                if tool_call_id == "tool-1")
        );
        assert!(
            matches!(&conversation.messages[1], Message::Tool { tool_call_id, .. }
                if tool_call_id == "tool-2")
        );
        assert!(combined_tool_content.contains("hello"));
    }

    #[test]
    fn run_readonly_tool_turn_returns_cancelled_after_closing_batch() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("one.txt"), "one\n").expect("write file");
        std::fs::write(cwd.path().join("two.txt"), "two\n").expect("write file");
        let requests = ["one.txt", "two.txt"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| ToolRequest {
                id: format!("tool-{}", index + 1),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some(path.to_string()),
                raw_arguments: Some(json!({ "path": path }).to_string()),
            })
            .collect::<Vec<_>>();
        let mut events = EventFactory::new("readonly-tool-turn-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
            request: RuntimeReadonlyToolTurnRequest {
                cwd: cwd.path(),
                tool_requests: &requests,
                emit_deltas: false,
                cancel: &cancel,
                output_truncation: ToolConfig::default().output_truncation,
                max_parallel: 2,
            },
            io: RuntimeReadonlyToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
            },
            services: RuntimeReadonlyToolTurnServices {
                mcp_registry: &registry,
                hooks: &hooks,
                provider_response_ingress: None,
                task_registry: None,
                root_task_id: None,
            },
        })
        .expect("run cancelled readonly tool turn");

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
                Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == ToolStatus::Cancelled
                    && terminal.started == ToolInvocationStarted::No
            ));
        }
    }

    #[test]
    fn run_readonly_tool_turn_records_pre_hook_failures_without_panicking() {
        let cwd = tempfile::tempdir().expect("cwd");
        let requests = ["one.txt", "two.txt"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| ToolRequest {
                id: format!("tool-{}", index + 1),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some(path.to_string()),
                raw_arguments: Some(json!({ "path": path }).to_string()),
            })
            .collect::<Vec<_>>();
        let mut events = EventFactory::new("readonly-tool-turn-hook-failure".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let registry = McpRegistry::default();
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "printf blocked >&2; exit 7".to_string(),
            tool: Some("read_file".to_string()),
        }]);
        let cancel = CancelToken::new();

        let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
            request: RuntimeReadonlyToolTurnRequest {
                cwd: cwd.path(),
                tool_requests: &requests,
                emit_deltas: false,
                cancel: &cancel,
                output_truncation: ToolConfig::default().output_truncation,
                max_parallel: 2,
            },
            io: RuntimeReadonlyToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
            },
            services: RuntimeReadonlyToolTurnServices {
                mcp_registry: &registry,
                hooks: &hooks,
                provider_response_ingress: None,
                task_registry: None,
                root_task_id: None,
            },
        })
        .expect("run readonly hook failures");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 2);
        for message in &conversation.messages {
            assert!(matches!(
                message,
                Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == ToolStatus::Failed
                    && terminal.started == ToolInvocationStarted::No
            ));
        }
    }

    #[test]
    fn run_readonly_tool_turn_cancels_blocked_pre_hook_before_tool_start() {
        let cwd = tempfile::tempdir().expect("cwd");
        let requests = vec![request(
            ToolName::ReadFile,
            ActionKind::Read,
            Some("one.txt"),
            Some(json!({ "path": "one.txt" }).to_string().as_str()),
        )];
        let mut events = EventFactory::new("readonly-tool-turn-hook-cancelled".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let registry = McpRegistry::default();
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "sleep 5".to_string(),
            tool: Some("read_file".to_string()),
        }]);
        let cancel = CancelToken::new();
        let cancel_from_thread = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel_from_thread.cancel();
        });
        let started = Instant::now();

        let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
            request: RuntimeReadonlyToolTurnRequest {
                cwd: cwd.path(),
                tool_requests: &requests,
                emit_deltas: false,
                cancel: &cancel,
                output_truncation: ToolConfig::default().output_truncation,
                max_parallel: 2,
            },
            io: RuntimeReadonlyToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
            },
            services: RuntimeReadonlyToolTurnServices {
                mcp_registry: &registry,
                hooks: &hooks,
                provider_response_ingress: None,
                task_registry: None,
                root_task_id: None,
            },
        })
        .expect("cancel blocked readonly pre-hook");
        canceller.join().expect("join canceller");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelled pre-hook took {:?}",
            started.elapsed()
        );
        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn run_tool_turns_records_policy_failure_and_closes_unstarted_siblings() {
        let cwd = tempfile::tempdir().expect("cwd");
        let config = config_with_external(Vec::new());
        let mut events = EventFactory::new("tool-turns-disallowed".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let allowed = vec!["read_file".to_string()];
        let requests = vec![
            request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None),
            ToolRequest {
                id: "tool-2".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf should-not-run".to_string()),
                raw_arguments: Some(r#"{"command":"printf should-not-run"}"#.to_string()),
            },
            ToolRequest {
                id: "tool-3".to_string(),
                name: ToolName::WriteFile,
                action: ActionKind::Write,
                target: Some("never.txt".to_string()),
                raw_arguments: Some(r#"{"path":"never.txt","content":"never"}"#.to_string()),
            },
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turns-disallowed".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            1,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        })
        .expect("run tool turns");

        match outcome {
            ToolTurnOutcome::Return {
                status,
                error,
                terminal: _,
            } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(
                    error.as_deref(),
                    Some("test child disallows tool 'subagent'")
                );
            }
            ToolTurnOutcome::Continue => panic!("disallowed child tool should end the turn"),
        }
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(conversation.messages.len(), 3);
        for (index, expected_id) in ["tool-1", "tool-2", "tool-3"].iter().enumerate() {
            let Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } = &conversation.messages[index]
            else {
                panic!("expected terminal-aware tool message at {index}")
            };
            assert_eq!(tool_call_id, expected_id);
            assert_eq!(terminal.started, ToolInvocationStarted::No);
            assert_eq!(
                terminal.status,
                if index == 0 {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Cancelled
                }
            );
        }

        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        let events = emitted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for id in ["tool-1", "tool-2", "tool-3"] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event["type"] == "tool.call.requested" && event["payload"]["id"] == id
                    })
                    .count(),
                1,
                "missing or duplicate requested event for {id}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event["type"] == "tool.call.completed" && event["payload"]["id"] == id
                    })
                    .count(),
                1,
                "missing or duplicate completed event for {id}"
            );
        }
    }

    #[test]
    fn run_tool_turns_closes_requests_after_subagent_batch_event_io_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.output_format = OutputFormat::Text;
        config.subagents.max_parallel = 2;
        let mut events = EventFactory::new("tool-turn-batch-event-error".to_string());
        let mut sink = EventSink::new(FailThirdFlush::default(), OutputFormat::Text);
        let mut conversation = Conversation::new();
        let requests = ["one", "two", "three"]
            .into_iter()
            .map(|id| ToolRequest {
                id: id.to_string(),
                name: ToolName::Subagent,
                action: ActionKind::Agent,
                target: Some(format!("inspect {id}")),
                raw_arguments: Some(
                    json!({
                        "description": format!("inspect {id}"),
                        "prompt": format!("inspect {id}")
                    })
                    .to_string(),
                ),
            })
            .collect::<Vec<_>>();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-batch-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let error = match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: delayed_batch_child_executor::<io::Sink>,
            },
        }) {
            Err(error) => error,
            Ok(_) => panic!("event I/O error must remain visible to the caller"),
        };

        assert!(error.to_string().contains("event consumer disconnected"));
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(conversation.messages.len(), 3);
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Completed
        ));
        assert!(matches!(
            &conversation.messages[1],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Failed
                && terminal.started == ToolInvocationStarted::No
        ));
        assert!(matches!(
            &conversation.messages[2],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn run_tool_turns_preserves_known_current_result_after_event_io_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("one.txt"), "one\n").expect("write fixture");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.output_format = OutputFormat::Text;
        config.tools.max_read_parallel = 1;
        let mut events = EventFactory::new("tool-turn-normal-event-error".to_string());
        let mut sink = EventSink::new(FailSecondFlush::default(), OutputFormat::Text);
        let mut conversation = Conversation::new();
        let requests = vec![
            ToolRequest {
                id: "read-one".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("one.txt".to_string()),
                raw_arguments: Some(r#"{"path":"one.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "never-run".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf never".to_string()),
                raw_arguments: Some(r#"{"command":"printf never"}"#.to_string()),
            },
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-normal-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let error = match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        }) {
            Err(error) => error,
            Ok(_) => panic!("event I/O error must remain visible to the caller"),
        };

        assert!(
            error
                .to_string()
                .contains("tool event consumer disconnected")
        );
        assert_eq!(sampling_state.tool_cursor_position(), 2);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Completed
        ));
        assert!(matches!(
            &conversation.messages[1],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn run_tool_turns_preserves_invalid_input_after_pre_dispatch_event_io_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.output_format = OutputFormat::Text;
        config.tools.max_read_parallel = 1;
        let mut events = EventFactory::new("tool-turn-invalid-event-error".to_string());
        let mut sink = EventSink::new(FailSecondFlush::default(), OutputFormat::Text);
        let mut conversation = Conversation::new();
        let requests = vec![
            ToolRequest {
                id: "invalid-read".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{}"#.to_string()),
            },
            ToolRequest {
                id: "never-run".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf never".to_string()),
                raw_arguments: Some(r#"{"command":"printf never"}"#.to_string()),
            },
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-invalid-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let error = match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        }) {
            Err(error) => error,
            Ok(_) => panic!("event I/O error must remain visible to the caller"),
        };

        assert!(
            error
                .to_string()
                .contains("tool event consumer disconnected")
        );
        assert_eq!(sampling_state.tool_cursor_position(), 2);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Failed
                && terminal.started == ToolInvocationStarted::No
        ));
        assert!(matches!(
            &conversation.messages[1],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn run_tool_turns_preserves_readonly_batch_results_after_event_io_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("one.txt"), "one\n").expect("write first fixture");
        std::fs::write(cwd.path().join("two.txt"), "two\n").expect("write second fixture");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.output_format = OutputFormat::Text;
        config.tools.max_read_parallel = 2;
        let mut events = EventFactory::new("tool-turn-readonly-event-error".to_string());
        let mut sink = EventSink::new(FailThirdFlush::default(), OutputFormat::Text);
        let mut conversation = Conversation::new();
        let requests = vec![
            ToolRequest {
                id: "read-one".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("one.txt".to_string()),
                raw_arguments: Some(r#"{"path":"one.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "read-two".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("two.txt".to_string()),
                raw_arguments: Some(r#"{"path":"two.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "never-run".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf never".to_string()),
                raw_arguments: Some(r#"{"command":"printf never"}"#.to_string()),
            },
        ];
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-readonly-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let error = match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        }) {
            Err(error) => error,
            Ok(_) => panic!("event I/O error must remain visible to the caller"),
        };

        assert!(error.to_string().contains("event consumer disconnected"));
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(conversation.messages.len(), 3);
        for message in &conversation.messages[..2] {
            assert!(matches!(
                message,
                Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == ToolStatus::Completed
            ));
        }
        assert!(matches!(
            &conversation.messages[2],
            Message::Tool {
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn run_tool_turns_closes_readonly_batch_and_sibling_after_history_failure() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("one.txt"), "one\n").expect("write first fixture");
        std::fs::write(cwd.path().join("two.txt"), "two\n").expect("write second fixture");
        let (_history, mut history_writer) =
            history_writer_that_fails_on_append("readonly sibling history failure");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.tools.max_read_parallel = 2;
        let requests = vec![
            ToolRequest {
                id: "read-one".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("one.txt".to_string()),
                raw_arguments: Some(r#"{"path":"one.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "read-two".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("two.txt".to_string()),
                raw_arguments: Some(r#"{"path":"two.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "never-run".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("printf never".to_string()),
                raw_arguments: Some(r#"{"command":"printf never"}"#.to_string()),
            },
        ];
        let mut events = EventFactory::new("tool-turn-history-error".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-history-error".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let error = match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: Some(&mut history_writer),
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        }) {
            Err(error) => error,
            Ok(_) => panic!("history append failure must remain visible"),
        };

        assert!(error.raw_os_error().is_some());
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| match message {
                    Message::Tool {
                        tool_call_id,
                        terminal: Some(terminal),
                        ..
                    } => (tool_call_id.as_str(), terminal.status),
                    _ => panic!("expected terminal-aware tool result"),
                })
                .collect::<Vec<_>>(),
            [
                ("read-one", ToolStatus::Completed),
                ("read-two", ToolStatus::Completed),
                ("never-run", ToolStatus::Cancelled),
            ]
        );
    }

    #[test]
    fn tool_turn_closes_unstarted_siblings_on_cancel() {
        struct CancelHandler {
            calls: Cell<usize>,
        }

        impl RuntimeUserInputHandler for CancelHandler {
            fn request_user_input(
                &self,
                _request: &crate::lifecycle::RuntimeUserInputRequest,
            ) -> io::Result<Option<String>> {
                self.calls.set(self.calls.get() + 1);
                Ok(None)
            }
        }

        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("tool-turn-cancel-siblings".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let requests = ["tool-1", "tool-2", "tool-3"]
            .into_iter()
            .map(|id| ToolRequest {
                id: id.to_string(),
                name: ToolName::AskUserQuestion,
                action: ActionKind::Read,
                target: Some("Continue?".to_string()),
                raw_arguments: Some(
                    r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#
                        .to_string(),
                ),
            })
            .collect::<Vec<_>>();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turn-cancel-siblings".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let handler = CancelHandler {
            calls: Cell::new(0),
        };
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::new(None, None),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            Some(&handler),
            None,
        );

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        })
        .expect("run cancelled tool turn");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert_eq!(
            handler.calls.get(),
            1,
            "unstarted siblings must not execute"
        );
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(conversation.messages.len(), 3);
        for (index, message) in conversation.messages.iter().enumerate() {
            let Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } = message
            else {
                panic!("expected terminal-aware tool message at {index}")
            };
            assert_eq!(tool_call_id, &requests[index].id);
            assert_eq!(terminal.status, ToolStatus::Cancelled);
            assert_eq!(
                terminal.started,
                if index == 0 {
                    ToolInvocationStarted::Yes
                } else {
                    ToolInvocationStarted::No
                }
            );
        }
        assert_one_terminal_event_pair_per_call(&mut sink, &requests);
    }

    #[test]
    fn tool_turn_closes_all_pending_calls_when_cancelled_before_dispatch() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("tool-turn-pre-cancel".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let requests = ["tool-1", "tool-2", "tool-3"]
            .into_iter()
            .map(|id| ToolRequest {
                id: id.to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("sleep 5".to_string()),
                raw_arguments: Some(r#"{"command":"sleep 5"}"#.to_string()),
            })
            .collect::<Vec<_>>();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        cancel.cancel();
        let task_registry = TaskRegistry::new("tool-turn-pre-cancel".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::new(None, None),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        })
        .expect("run pre-cancelled tool turn");

        assert!(matches!(
            outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Cancelled,
                ..
            }
        ));
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert_eq!(conversation.messages.len(), 3);
        for message in &conversation.messages {
            assert!(matches!(
                message,
                Message::Tool {
                    terminal: Some(terminal),
                    ..
                } if terminal.status == ToolStatus::Cancelled
                    && terminal.started == ToolInvocationStarted::No
            ));
        }
        assert_one_terminal_event_pair_per_call(&mut sink, &requests);
        assert!(task_registry.list().is_empty(), "no shell task may start");
    }

    fn assert_one_terminal_event_pair_per_call(
        sink: &mut EventSink<Vec<u8>>,
        requests: &[ToolRequest],
    ) {
        let emitted = String::from_utf8(sink.writer_mut().clone()).expect("jsonl events");
        let events = emitted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for request in requests {
            for event_type in ["tool.call.requested", "tool.call.completed"] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| {
                            event["type"] == event_type && event["payload"]["id"] == request.id
                        })
                        .count(),
                    1,
                    "expected one {event_type} event for {}",
                    request.id
                );
            }
        }
    }

    #[test]
    fn sequential_subagents_stop_after_first_child_crosses_budget() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        config.budget.max_cost_usd_micros = Some(1_000_000);
        let subagent_request = |id: &str, prompt: &str| ToolRequest {
            id: id.to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some(prompt.to_string()),
            raw_arguments: Some(
                json!({
                    "description": prompt,
                    "prompt": prompt
                })
                .to_string(),
            ),
        };
        let requests = [
            subagent_request("child-1", "first child"),
            subagent_request("child-2", "second child"),
        ];
        let mut events = EventFactory::new("sequential-subagent-budget".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("sequential-subagent-budget".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            operation: None,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: budget_crossing_child_executor::<io::Sink>,
            },
        })
        .expect("run sequential subagent turns");

        match outcome {
            ToolTurnOutcome::Return {
                status,
                error,
                terminal: _,
            } => {
                assert_eq!(status, RunStatus::Failed);
                assert!(
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("budget stopped"))
                );
            }
            ToolTurnOutcome::Continue => panic!("budget crossing must stop the tool turn"),
        }
        assert_eq!(cost_tracker.totals().input_tokens, 10_000_000);
        assert!(cost_tracker.totals().estimated_cost_usd > 1.0);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            &conversation.messages[0],
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if tool_call_id == "child-1"
                && terminal.status == ToolStatus::Completed
                && terminal.started == ToolInvocationStarted::Yes
        ));
        assert!(matches!(
            &conversation.messages[1],
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if tool_call_id == "child-2"
                && terminal.status == ToolStatus::Cancelled
                && terminal.started == ToolInvocationStarted::No
        ));
        let output = String::from_utf8(sink.writer_mut().clone()).expect("jsonl is utf8");
        assert_eq!(output.matches("\"type\":\"usage.updated\"").count(), 1);
        assert_one_terminal_event_pair_per_call(&mut sink, &requests);

        let mut admission_state = RuntimeSamplingRequestState::new();
        let mut admission_conversation = Conversation::new();
        let admission_outcome = run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state: &mut admission_state,
            request: RuntimeNormalToolTurnRequest {
                config: &config,
                cwd: cwd.path(),
                tool_request: &requests[1],
                subagent_depth: 0,
                emit_deltas: false,
                goal_mode: false,
                policy: &policy,
                root_task_id: None,
                child_budget: None,
            },
            io: RuntimeNormalToolTurnIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut admission_conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            services: RuntimeNormalToolTurnServices {
                instructions: &instructions,
                memory: &memory,
                mcp_registry: &mcp_registry,
                hooks: &hooks,
            },
            runtime: RuntimeNormalToolTurnRuntime {
                cancel: &cancel,
                task_registry: &task_registry,
                workflow_ipc: None,
                workflow_lifecycle_ingress: None,
                wait_for_background_workflows: true,
            },
            interactions: RuntimeNormalToolTurnInteractions {
                approval_handler: None,
                permission_handler: None,
                user_input_handler: None,
                mcp_elicitation_handler: None,
                provider_response_ingress: None,
            },
            extensions: None,
            executors: RuntimeNormalToolTurnExecutors {
                subagent_child_executor: unused_child_executor::<io::Sink>,
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
            },
        })
        .expect("reject already exhausted child admission");

        assert!(matches!(
            admission_outcome.outcome,
            ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                ..
            }
        ));
        assert!(admission_outcome.event_error.is_none());
        assert!(matches!(
            admission_conversation.messages.as_slice(),
            [Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            }] if tool_call_id == "child-2"
                && terminal.status == ToolStatus::Failed
                && terminal.started == ToolInvocationStarted::No
        ));
    }

    #[test]
    fn terminal_tool_turn_carries_status_and_optional_error() {
        match terminal_tool_turn(RunStatus::Failed, Some("tool failed".to_string())) {
            ToolTurnOutcome::Return {
                status,
                error,
                terminal: _,
            } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("tool failed"));
            }
            ToolTurnOutcome::Continue => panic!("terminal tool turn must return"),
        }
    }
}
