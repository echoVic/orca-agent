use std::io;
use std::path::Path;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_mcp::{McpElicitationHandler, McpRegistry};

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::extension::RuntimeExtensionStores;
use crate::goal_actor::{GoalRuntimeHandle, GoalTurnContext};
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeToolActorContext, RuntimeUserInputHandler,
    RuntimeWorkflowIpc, TurnPermissionOverlay,
};
use crate::memory::MemoryBlock;
use crate::runtime_special::{
    RuntimeGoalToolOutcome, RuntimeGoalToolRequest, RuntimeSpecialToolDispatch,
};
use crate::runtime_state::RuntimeTurnReducer;
use crate::runtime_surface::RuntimeWorkflowLifecycleIngress;
use crate::runtime_tool_call::{
    RuntimeNormalToolInteractions, RuntimeNormalToolInvocation, RuntimeToolCallRuntime,
};
use crate::subagent_execution::execute_subagent_tool_with_activity_ingress;
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::{
    BackgroundWorkflowRun, execute_workflow_draft_action_tool, execute_workflow_tool,
};

pub(crate) struct RuntimeToolInvocationContext<'a, W: io::Write> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) execution_request: &'a tool_types::ToolRequest,
    pub(crate) goal_mode: bool,
    pub(crate) subagent_depth: u32,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) emit_deltas: bool,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) permission_handler_owned:
        Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    pub(crate) user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    pub(crate) mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    pub(crate) extension_stores: Option<RuntimeExtensionStores<'a>>,
    pub(crate) goal_runtime: Option<GoalRuntimeHandle>,
    pub(crate) goal_turn: Option<GoalTurnContext>,
    pub(crate) event_error: &'a mut Option<io::Error>,
    pub(crate) subagent_child_executor: ChildAgentExecutor<io::Sink>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) workflow_lifecycle_ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    pub(crate) wait_for_background_workflows: bool,
    pub(crate) root_task_id: Option<&'a str>,
    /// Lease-derived budget bound for subagent children of this tool call
    /// (parent remaining minus outstanding reservations).
    pub(crate) child_budget: Option<orca_core::budget::BudgetSpec>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeToolTurnDisposition {
    ContinueModel,
    StopTurn,
}

pub(crate) struct RuntimeToolDispatchOutput {
    pub(crate) result: tool_types::ToolResult,
    pub(crate) disposition: RuntimeToolTurnDisposition,
    /// The subagent child's consumed budget receipt, when this dispatch ran
    /// a child agent that reported one.
    pub(crate) child_budget_usage: Option<orca_core::budget::BudgetUsage>,
}

impl RuntimeToolDispatchOutput {
    fn continue_model(result: tool_types::ToolResult) -> Self {
        Self {
            result,
            disposition: RuntimeToolTurnDisposition::ContinueModel,
            child_budget_usage: None,
        }
    }

    fn stop_turn(result: tool_types::ToolResult) -> Self {
        Self {
            result,
            disposition: RuntimeToolTurnDisposition::StopTurn,
            child_budget_usage: None,
        }
    }
}

pub(crate) struct RuntimeToolRouter<'a> {
    runtime: &'a mut RuntimeToolActorContext,
    tool_calls: RuntimeToolCallRuntime,
}

impl<'a> RuntimeToolRouter<'a> {
    pub(crate) fn new(runtime: &'a mut RuntimeToolActorContext) -> Self {
        Self {
            runtime,
            tool_calls: RuntimeToolCallRuntime::for_normal_execution(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_tool_call_runtime(
        runtime: &'a mut RuntimeToolActorContext,
        tool_calls: RuntimeToolCallRuntime,
    ) -> Self {
        Self {
            runtime,
            tool_calls,
        }
    }

    pub(crate) fn dispatch<W: io::Write>(
        &mut self,
        context: RuntimeToolInvocationContext<'_, W>,
    ) -> io::Result<RuntimeToolDispatchOutput> {
        let RuntimeToolInvocationContext {
            config,
            cwd,
            events,
            sink,
            execution_request,
            goal_mode,
            subagent_depth,
            instructions,
            memory,
            mcp_registry,
            hooks,
            emit_deltas,
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_overlay,
            permission_handler,
            permission_handler_owned,
            user_input_handler,
            mcp_elicitation_handler,
            extension_stores,
            goal_runtime,
            goal_turn,
            event_error,
            subagent_child_executor,
            workflow_child_executor,
            workflow_lifecycle_ingress,
            wait_for_background_workflows,
            root_task_id,
            child_budget,
        } = context;

        let mut dispatch_child_budget_usage = None;
        let result = match self.runtime.classify_dispatch(execution_request, goal_mode) {
            RuntimeSpecialToolDispatch::GetGoal
            | RuntimeSpecialToolDispatch::CreateGoal
            | RuntimeSpecialToolDispatch::UpdateGoal => {
                let persistent_session_id = (!matches!(config.history_mode, HistoryMode::Disabled)
                    && task_registry.session_id() == events.run_id())
                .then_some(task_registry.session_id());
                return Ok(
                    match self.runtime.execute_goal_tool(
                        execution_request,
                        RuntimeGoalToolRequest {
                            persistent_session_id,
                            goal_runtime,
                            goal_turn,
                            events,
                            sink,
                            event_error,
                        },
                    ) {
                        RuntimeGoalToolOutcome::Continue(result) => {
                            RuntimeToolDispatchOutput::continue_model(result)
                        }
                        RuntimeGoalToolOutcome::StopTurn(result) => {
                            RuntimeToolDispatchOutput::stop_turn(result)
                        }
                    },
                );
            }
            RuntimeSpecialToolDispatch::WorkflowDraft => {
                self.runtime.execute_workflow_draft_tool_with_registry(
                    execution_request,
                    config.workflows.enabled,
                    cwd,
                    task_registry,
                    config.workflows.max_concurrent_agents,
                )
            }
            RuntimeSpecialToolDispatch::WorkflowDraftAction => execute_workflow_draft_action_tool(
                config,
                cwd,
                events,
                sink,
                execution_request,
                emit_deltas,
                task_registry,
                background_workflows,
                workflow_child_executor,
                wait_for_background_workflows,
                workflow_lifecycle_ingress,
            ),
            RuntimeSpecialToolDispatch::Workflow => execute_workflow_tool(
                config,
                cwd,
                events,
                sink,
                execution_request,
                emit_deltas,
                task_registry,
                background_workflows,
                workflow_child_executor,
                wait_for_background_workflows,
                workflow_lifecycle_ingress,
            ),
            RuntimeSpecialToolDispatch::Subagent => {
                let (result, child_budget_usage) = execute_subagent_tool_with_activity_ingress(
                    config,
                    cwd,
                    events,
                    sink,
                    execution_request,
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
                    subagent_child_executor,
                    permission_handler_owned,
                    workflow_lifecycle_ingress
                        .and_then(|ingress| ingress.subagent_activity_ingress()),
                    event_error,
                    child_budget.as_ref(),
                )?;
                dispatch_child_budget_usage = child_budget_usage;
                Ok(result)
            }
            RuntimeSpecialToolDispatch::SubagentStatus => Ok(self
                .runtime
                .execute_subagent_status_tool(execution_request, task_registry)),
            RuntimeSpecialToolDispatch::TaskList => Ok(self
                .runtime
                .execute_task_list_tool(execution_request, task_registry)),
            RuntimeSpecialToolDispatch::TaskStop => {
                let result = self
                    .runtime
                    .execute_task_stop_tool(execution_request, task_registry);
                if result.status == tool_types::ToolStatus::Completed
                    && let Some(task_id) = task_stop_id(execution_request)
                {
                    let thread_store = extension_stores
                        .map(|stores| stores.thread_store())
                        .unwrap_or(&self.runtime.thread_extensions);
                    if let Some(service) =
                        thread_store.get::<crate::terminal_service::TerminalService>()
                        && let Err(error) = service.stop_task(&task_id)
                    {
                        return Ok(RuntimeToolDispatchOutput::continue_model(
                            tool_types::ToolResult::failed_after_start(
                                execution_request,
                                format!("failed to stop terminal task '{task_id}': {error}"),
                                None,
                            ),
                        ));
                    }
                }
                Ok(result)
            }
            RuntimeSpecialToolDispatch::RequestPermissions => {
                let result = self.runtime.execute_request_permissions_tool_with_policy(
                    execution_request,
                    config.approval_mode,
                    permission_handler
                        .map(|handler| handler as &dyn RuntimePermissionRequestHandler),
                );
                let extension_stores = extension_stores.unwrap_or_else(|| {
                    RuntimeExtensionStores::new(
                        &self.runtime.thread_extensions,
                        &self.runtime.turn_extensions,
                    )
                });
                let reducer = RuntimeTurnReducer::from_extension_stores(extension_stores);
                reducer.merge_permission_overlay(
                    permission_overlay,
                    self.runtime.permission_overlay(),
                );
                Ok(result)
            }
            RuntimeSpecialToolDispatch::RequestUserInput => {
                let Some(user_input_handler) = user_input_handler else {
                    return Ok(RuntimeToolDispatchOutput::continue_model(
                        tool_types::ToolResult::failed(
                            execution_request,
                            format!(
                                "{} requires a runtime user input handler",
                                execution_request.name.as_str()
                            ),
                            None,
                        ),
                    ));
                };
                self.runtime
                    .execute_user_input_tool(execution_request, user_input_handler)
            }
            RuntimeSpecialToolDispatch::WorkflowIpc => Ok(self.runtime.execute_workflow_ipc_tool(
                execution_request,
                workflow_ipc.map(|ipc| ipc as &dyn RuntimeWorkflowIpc),
            )),
            RuntimeSpecialToolDispatch::Normal => {
                let additional_roots = config
                    .additional_working_directories
                    .iter()
                    .filter(|directory| {
                        directory.source
                            != crate::runtime_permission::SESSION_METADATA_DIRECTORY_SOURCE
                    })
                    .map(|directory| directory.path.clone())
                    .chain(
                        permission_overlay
                            .additional_working_directories()
                            .iter()
                            .cloned(),
                    )
                    .collect::<Vec<_>>();
                let terminal_service = matches!(
                    execution_request.name,
                    tool_types::ToolName::ExecCommand | tool_types::ToolName::WriteStdin
                )
                .then(|| {
                    let thread_store = extension_stores
                        .map(|stores| stores.thread_store())
                        .unwrap_or(&self.runtime.thread_extensions);
                    thread_store.get_or_init(|| {
                        crate::terminal_service::TerminalService::new(task_registry.clone())
                    })
                });
                let invocation = RuntimeNormalToolInvocation::snapshot(
                    config,
                    execution_request,
                    cwd,
                    &additional_roots,
                    mcp_registry,
                    &config.external_tools,
                    config.tools.output_truncation,
                    config.tools.shell_timeout_secs,
                    Some(task_registry),
                    permission_overlay.clone(),
                )
                .with_terminal_service(terminal_service);
                let output = {
                    let mut output_handler = |chunk: &str| {
                        sink.emit(events.tool_output_delta(&execution_request.id, chunk))
                    };
                    self.tool_calls.execute_normal(
                        invocation,
                        cancel,
                        RuntimeNormalToolInteractions {
                            output_handler: emit_deltas.then_some(&mut output_handler),
                            permission_handler: permission_handler
                                .map(|handler| handler as &dyn RuntimePermissionRequestHandler),
                            mcp_elicitation_handler: mcp_elicitation_handler
                                .map(|handler| handler as &dyn McpElicitationHandler),
                        },
                    )?
                };
                let extension_stores = extension_stores.unwrap_or_else(|| {
                    RuntimeExtensionStores::new(
                        &self.runtime.thread_extensions,
                        &self.runtime.turn_extensions,
                    )
                });
                RuntimeTurnReducer::from_extension_stores(extension_stores)
                    .merge_permission_delta(permission_overlay, &output.permission_delta);
                if event_error.is_none() {
                    *event_error = output.event_error;
                }
                Ok(output.result)
            }
        }?;
        Ok(RuntimeToolDispatchOutput {
            result,
            disposition: RuntimeToolTurnDisposition::ContinueModel,
            child_budget_usage: dispatch_child_budget_usage,
        })
    }
}

fn task_stop_id(request: &tool_types::ToolRequest) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(request.raw_arguments.as_deref()?).ok()?;
    args.get("task_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| args.get("shell_id").and_then(serde_json::Value::as_str))
        .filter(|task_id| !task_id.trim().is_empty())
        .map(str::to_string)
}
