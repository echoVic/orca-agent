use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::ApprovalDecision;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_mcp::{McpElicitationHandler, McpRegistry};

use crate::agent_child::ChildAgentExecutor;
use crate::agent_common;
use crate::cost::CostTracker;
use crate::extension::{
    ExtensionRegistry, RuntimeExtensionStores, ToolCallOutcome, ToolFinishInput, ToolStartInput,
};
use crate::goal_actor::{GoalRuntimeBinding, GoalRuntimeHandle, GoalTurnContext};
use crate::hooks::{HookOutcome, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimeConfigApprovalHandler,
    RuntimePermissionRequestHandler, RuntimeTaskActor, RuntimeToolActorContext,
    RuntimeToolApprovalPolicy, RuntimeUserInputHandler, TurnPermissionOverlay,
    run_status_from_tool_status,
};
use crate::memory::MemoryBlock;
use crate::runtime_surface::{RuntimeProviderResponseIngress, RuntimeWorkflowLifecycleIngress};
use crate::tasks::TaskRegistry;
use crate::tool_invocation::{
    ToolInvocation, apply_pre_tool_outcome, approval_request_for_invocation,
    prepare_tool_invocation, validate_tool_invocation,
};
use crate::tool_router::{
    RuntimeToolDispatchOutput, RuntimeToolInvocationContext, RuntimeToolRouter,
    RuntimeToolTurnDisposition,
};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

#[derive(Debug)]
struct RuntimeSemanticCommitFailure {
    message: String,
}

impl std::fmt::Display for RuntimeSemanticCommitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeSemanticCommitFailure {}

pub(crate) fn semantic_commit_failure(error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        RuntimeSemanticCommitFailure {
            message: error.to_string(),
        },
    )
}

pub(crate) fn is_semantic_commit_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RuntimeSemanticCommitFailure>())
        .is_some()
}

pub(crate) struct ToolExecutionContext<'a> {
    cwd: &'a Path,
    subagent_depth: u32,
    emit_deltas: bool,
    goal_mode: bool,
    policy: &'a ApprovalPolicy,
    instructions: Option<&'a ProjectInstructions>,
    memory: Option<&'a MemoryBlock>,
    mcp_registry: Option<&'a McpRegistry>,
    hooks: Option<&'a HookRunner>,
    cost_tracker: Option<&'a mut CostTracker>,
    cancel: Option<&'a CancelToken>,
    task_registry: Option<&'a TaskRegistry>,
    background_workflows: Option<&'a mut Vec<BackgroundWorkflowRun>>,
    workflow_ipc: Option<&'a WorkflowIpcContext>,
    permission_overlay: Option<&'a mut TurnPermissionOverlay>,
    approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    workflow_lifecycle_ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    wait_for_background_workflows: bool,
    extension_registry: Option<&'a ExtensionRegistry>,
    extension_stores: Option<RuntimeExtensionStores<'a>>,
    goal_runtime: Option<GoalRuntimeHandle>,
    goal_turn: Option<GoalTurnContext>,
    root_task_id: Option<&'a str>,
    /// Lease-derived budget bound for subagent children of this tool call
    /// (parent remaining minus outstanding reservations).
    child_budget: Option<orca_core::budget::BudgetSpec>,
}

pub(crate) struct ToolApprovalGateContext<'a, W: io::Write> {
    pub(crate) config: &'a RunConfig,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) tool_request: &'a tool_types::ToolRequest,
    pub(crate) invocation: &'a ToolInvocation,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) emit_deltas: bool,
    pub(crate) provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
}

struct PreToolHookExecution {
    outcome: Result<ToolInvocation, (RunStatus, tool_types::ToolResult)>,
    event_error: Option<io::Error>,
}

#[derive(Default)]
pub(crate) struct ApprovalGateExecution {
    pub(crate) outcome: Option<(RunStatus, tool_types::ToolResult)>,
    pub(crate) event_error: Option<io::Error>,
}

pub(crate) struct ToolExecutionActor {
    runtime: RuntimeToolActorContext,
}

pub(crate) struct ToolExecutionCompletion {
    pub(crate) status: RunStatus,
    pub(crate) result: tool_types::ToolResult,
    pub(crate) disposition: RuntimeToolTurnDisposition,
    pub(crate) event_error: Option<io::Error>,
    /// The subagent child's consumed budget receipt, when this tool ran a
    /// child agent that reported one.
    pub(crate) child_budget_usage: Option<orca_core::budget::BudgetUsage>,
}

impl ToolExecutionCompletion {
    fn from_pair((status, result): (RunStatus, tool_types::ToolResult)) -> Self {
        Self {
            status,
            result,
            disposition: RuntimeToolTurnDisposition::ContinueModel,
            event_error: None,
            child_budget_usage: None,
        }
    }

    fn from_pair_with_event_error(
        pair: (RunStatus, tool_types::ToolResult),
        event_error: Option<io::Error>,
    ) -> Self {
        let mut completion = Self::from_pair(pair);
        completion.event_error = event_error;
        completion
    }
}

pub(crate) fn policy_for_tool_execution(config: &RunConfig) -> ApprovalPolicy {
    ApprovalPolicy::new(config.approval_mode).with_permission_rules(config.permission_rules.clone())
}

impl<'a> ToolExecutionContext<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
    ) -> Self {
        Self {
            cwd,
            subagent_depth,
            emit_deltas,
            goal_mode: false,
            policy,
            instructions: None,
            memory: None,
            mcp_registry: None,
            hooks: None,
            cost_tracker: None,
            cancel: None,
            task_registry: None,
            background_workflows: None,
            workflow_ipc: None,
            permission_overlay: None,
            approval_handler: None,
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
            provider_response_ingress: None,
            workflow_lifecycle_ingress: None,
            wait_for_background_workflows: true,
            extension_registry: None,
            extension_stores: None,
            goal_runtime: None,
            goal_turn: None,
            root_task_id: None,
            child_budget: None,
        }
    }

    pub(crate) fn with_child_budget(
        mut self,
        child_budget: Option<orca_core::budget::BudgetSpec>,
    ) -> Self {
        self.child_budget = child_budget;
        self
    }

    pub(crate) fn with_goal_mode(mut self, goal_mode: bool) -> Self {
        self.goal_mode = goal_mode;
        self
    }

    pub(crate) fn with_root_task_id(mut self, root_task_id: Option<&'a str>) -> Self {
        self.root_task_id = root_task_id;
        self
    }

    pub(crate) fn with_services(
        mut self,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        self.instructions = Some(instructions);
        self.memory = Some(memory);
        self.mcp_registry = Some(mcp_registry);
        self.hooks = Some(hooks);
        self
    }

    pub(crate) fn with_permission_overlay(
        mut self,
        permission_overlay: &'a mut TurnPermissionOverlay,
    ) -> Self {
        self.permission_overlay = Some(permission_overlay);
        self
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.permission_handler = permission_handler;
        self
    }

    pub(crate) fn with_approval_handler(
        mut self,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    ) -> Self {
        self.approval_handler = approval_handler;
        self
    }

    pub(crate) fn with_user_input_handler(
        mut self,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    ) -> Self {
        self.user_input_handler = user_input_handler;
        self
    }

    pub(crate) fn with_mcp_elicitation_handler(
        mut self,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        self.mcp_elicitation_handler = mcp_elicitation_handler;
        self
    }

    pub(crate) fn with_provider_response_ingress(
        mut self,
        provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    ) -> Self {
        self.provider_response_ingress = provider_response_ingress;
        self
    }

    pub(crate) fn with_workflow_lifecycle(
        mut self,
        ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
        wait_for_background_workflows: bool,
    ) -> Self {
        self.workflow_lifecycle_ingress = ingress;
        self.wait_for_background_workflows = wait_for_background_workflows;
        self
    }

    pub(crate) fn with_extensions(
        mut self,
        extension_registry: &'a ExtensionRegistry,
        extension_stores: RuntimeExtensionStores<'a>,
    ) -> Self {
        self.extension_registry = Some(extension_registry);
        self.extension_stores = Some(extension_stores);
        self
    }

    pub(crate) fn with_goal_runtime_binding(mut self, binding: GoalRuntimeBinding) -> Self {
        self.goal_runtime = Some(binding.handle);
        self.goal_turn = binding.turn;
        self
    }

    pub(crate) fn with_runtime(
        mut self,
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
    ) -> Self {
        self.cost_tracker = Some(cost_tracker);
        self.cancel = Some(cancel);
        self.task_registry = Some(task_registry);
        self.background_workflows = Some(background_workflows);
        self.workflow_ipc = workflow_ipc;
        self
    }

    #[cfg(test)]
    pub(crate) fn cwd(&self) -> &'a Path {
        self.cwd
    }

    #[cfg(test)]
    pub(crate) fn subagent_depth(&self) -> u32 {
        self.subagent_depth
    }

    #[cfg(test)]
    pub(crate) fn emit_deltas(&self) -> bool {
        self.emit_deltas
    }

    #[cfg(test)]
    pub(crate) fn policy(&self) -> &'a ApprovalPolicy {
        self.policy
    }

    #[cfg(test)]
    pub(crate) fn instructions(&self) -> &'a ProjectInstructions {
        self.instructions.expect("tool execution instructions")
    }

    #[cfg(test)]
    pub(crate) fn memory(&self) -> &'a MemoryBlock {
        self.memory.expect("tool execution memory")
    }

    #[cfg(test)]
    pub(crate) fn mcp_registry(&self) -> &'a McpRegistry {
        self.mcp_registry.expect("tool execution mcp registry")
    }

    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &'a HookRunner {
        self.hooks.expect("tool execution hooks")
    }

    #[cfg(test)]
    pub(crate) fn cost_tracker(&self) -> &CostTracker {
        self.cost_tracker
            .as_deref()
            .expect("tool execution cost tracker")
    }

    #[cfg(test)]
    pub(crate) fn cancel(&self) -> &'a CancelToken {
        self.cancel.expect("tool execution cancel token")
    }

    #[cfg(test)]
    pub(crate) fn task_registry(&self) -> &'a TaskRegistry {
        self.task_registry.expect("tool execution task registry")
    }

    #[cfg(test)]
    pub(crate) fn background_workflow_count(&self) -> usize {
        self.background_workflows
            .as_deref()
            .expect("tool execution background workflows")
            .len()
    }

    #[cfg(test)]
    pub(crate) fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.workflow_ipc
    }
}

pub(crate) fn execute_tool_with_approval<W: io::Write>(
    config: &RunConfig,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    context: ToolExecutionContext<'_>,
    subagent_child_executor: ChildAgentExecutor<io::Sink>,
    workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<ToolExecutionCompletion> {
    let mut actor = ToolExecutionActor::new(events.run_id().to_string());
    actor.execute_with_event_error(
        config,
        events,
        sink,
        tool_request,
        context,
        subagent_child_executor,
        workflow_child_executor,
    )
}

impl ToolExecutionActor {
    pub(crate) fn new(run_id: impl Into<String>) -> Self {
        Self {
            runtime: RuntimeToolActorContext::new(run_id),
        }
    }

    #[cfg(test)]
    pub(crate) fn active_task(&self) -> Option<&crate::lifecycle::RuntimeTaskLifecycle> {
        self.runtime.active_task()
    }

    fn run_pre_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &tool_types::ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, tool_types::ToolResult> {
        self.runtime
            .run_pre_tool_hook_with_cancel(hooks, cwd, request, cancel)
    }

    fn run_post_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &tool_types::ToolRequest,
        result: &tool_types::ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        self.runtime
            .run_post_tool_hook_with_cancel(hooks, cwd, request, result, cancel)
    }

    #[cfg(test)]
    pub(crate) fn execute<W: io::Write>(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        tool_request: &tool_types::ToolRequest,
        context: ToolExecutionContext<'_>,
        subagent_child_executor: ChildAgentExecutor<io::Sink>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    ) -> io::Result<(RunStatus, tool_types::ToolResult)> {
        let completion = self.execute_with_event_error(
            config,
            events,
            sink,
            tool_request,
            context,
            subagent_child_executor,
            workflow_child_executor,
        )?;
        match completion.event_error {
            Some(error) => Err(error),
            None => Ok((completion.status, completion.result)),
        }
    }

    fn execute_with_event_error<W: io::Write>(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        tool_request: &tool_types::ToolRequest,
        context: ToolExecutionContext<'_>,
        subagent_child_executor: ChildAgentExecutor<io::Sink>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    ) -> io::Result<ToolExecutionCompletion> {
        let ToolExecutionContext {
            cwd,
            subagent_depth,
            emit_deltas,
            goal_mode,
            policy,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_overlay,
            approval_handler,
            permission_handler,
            user_input_handler,
            mcp_elicitation_handler,
            provider_response_ingress,
            workflow_lifecycle_ingress,
            wait_for_background_workflows,
            extension_registry,
            extension_stores,
            goal_runtime,
            goal_turn,
            root_task_id,
            child_budget,
        } = context;
        let instructions = instructions.expect("tool execution instructions");
        let memory = memory.expect("tool execution memory");
        let mcp_registry = mcp_registry.expect("tool execution mcp registry");
        let hooks = hooks.expect("tool execution hooks");
        let cost_tracker = cost_tracker.expect("tool execution cost tracker");
        let cancel = cancel.expect("tool execution cancel token");
        let task_registry = task_registry.expect("tool execution task registry");
        let background_workflows =
            background_workflows.expect("tool execution background workflows");
        let permission_overlay = permission_overlay.expect("tool execution permission overlay");
        let invocation =
            prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
        if let Err(error) = validate_tool_invocation(&invocation, mcp_registry, config) {
            let result = error.into_result();
            let mut event_error = None;
            if emit_deltas {
                retain_first_io_error(
                    &mut event_error,
                    emit_tool_call_requested(events, sink, tool_request),
                );
            }
            retain_first_io_error(
                &mut event_error,
                publish_tool_call_completed(
                    events,
                    sink,
                    tool_request,
                    &result,
                    emit_deltas,
                    provider_response_ingress,
                ),
            );
            return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                (RunStatus::Failed, result),
                event_error,
            ));
        }

        let approval_bound_request = approval_request_for_invocation(&invocation)
            .filter(|approval| agent_common::requires_approval(approval.action))
            .map(|_| invocation.effective.clone());

        let approval_execution = self.handle_approval(ToolApprovalGateContext {
            config,
            events,
            sink,
            tool_request,
            invocation: &invocation,
            policy,
            permission_overlay,
            approval_handler,
            cancel,
            emit_deltas,
            provider_response_ingress,
        });
        match (approval_execution.outcome, approval_execution.event_error) {
            (Some(outcome), event_error) => {
                return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                    outcome,
                    event_error,
                ));
            }
            (None, Some(error)) => {
                let result = tool_types::ToolResult::failed_before_start(
                    tool_request,
                    format!(
                        "tool dispatch stopped because approval event delivery failed: {error}"
                    ),
                    None,
                );
                let mut event_error = Some(error);
                if emit_deltas {
                    retain_first_io_error(
                        &mut event_error,
                        emit_tool_call_requested(events, sink, tool_request),
                    );
                }
                retain_first_io_error(
                    &mut event_error,
                    publish_tool_call_completed(
                        events,
                        sink,
                        tool_request,
                        &result,
                        emit_deltas,
                        provider_response_ingress,
                    ),
                );
                return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                    (RunStatus::Failed, result),
                    event_error,
                ));
            }
            (None, None) => {}
        }

        if emit_deltas {
            let mut event_error = None;
            retain_first_io_error(
                &mut event_error,
                emit_tool_call_requested(events, sink, tool_request),
            );
            if event_error.is_some() {
                let result = tool_types::ToolResult::failed_before_start(
                    tool_request,
                    format!(
                        "tool dispatch stopped because its requested event could not be delivered: {}",
                        event_error.as_ref().expect("requested event error")
                    ),
                    None,
                );
                retain_first_io_error(
                    &mut event_error,
                    publish_tool_call_completed(
                        events,
                        sink,
                        tool_request,
                        &result,
                        emit_deltas,
                        provider_response_ingress,
                    ),
                );
                return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                    (RunStatus::Failed, result),
                    event_error,
                ));
            }
        }
        let cwd_display = cwd.display().to_string();
        let hook_execution = self.apply_pre_tool_hook(
            config,
            events,
            sink,
            tool_request,
            invocation,
            hooks,
            &cwd_display,
            mcp_registry,
            emit_deltas,
            provider_response_ingress,
            Some(cancel),
        );
        let invocation = match hook_execution.outcome {
            Ok(invocation) => invocation,
            Err(outcome) => {
                return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                    outcome,
                    hook_execution.event_error,
                ));
            }
        };
        if approval_bound_request.as_ref().is_some_and(|approved| {
            approved.id != invocation.effective.id
                || approved.name.as_str() != invocation.effective.name.as_str()
                || approved.action != invocation.effective.action
                || approved.target != invocation.effective.target
                || approved.raw_arguments != invocation.effective.raw_arguments
        }) {
            let result = tool_types::ToolResult::failed_before_start(
                tool_request,
                "pre-tool hook changed an approval-relevant request after its authority was evaluated",
                None,
            );
            let mut event_error = hook_execution.event_error;
            retain_first_io_error(
                &mut event_error,
                publish_tool_call_completed(
                    events,
                    sink,
                    tool_request,
                    &result,
                    emit_deltas,
                    provider_response_ingress,
                ),
            );
            return Ok(ToolExecutionCompletion::from_pair_with_event_error(
                (RunStatus::Failed, result),
                event_error,
            ));
        }
        let execution_request = &invocation.effective;
        if let (Some(registry), Some(extension_stores)) = (extension_registry, extension_stores) {
            registry.on_tool_start(ToolStartInput {
                thread_store: extension_stores.thread_store(),
                turn_store: extension_stores.turn_store(),
                tool_name: execution_request.name.as_str(),
                call_id: &execution_request.id,
            });
        }
        let mut dispatch_event_error = None;
        let dispatch = match RuntimeToolRouter::new(&mut self.runtime).dispatch(
            RuntimeToolInvocationContext {
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
                user_input_handler,
                mcp_elicitation_handler,
                extension_stores,
                goal_runtime,
                goal_turn,
                event_error: &mut dispatch_event_error,
                subagent_child_executor,
                workflow_child_executor,
                workflow_lifecycle_ingress,
                wait_for_background_workflows,
                root_task_id,
                child_budget,
            },
        ) {
            Ok(output) => output,
            Err(error) => RuntimeToolDispatchOutput {
                result: tool_types::ToolResult::indeterminate(
                    execution_request,
                    format!(
                        "Tool invocation outcome is indeterminate after an execution I/O error: {error}. Inspect external state before retrying."
                    ),
                ),
                disposition: RuntimeToolTurnDisposition::ContinueModel,
                child_budget_usage: None,
            },
        };
        let result = dispatch.result;
        // The subagent child's budget receipt rides to the turn settlement
        // point so the parent operation merges exactly what the child spent.
        let child_budget_usage = dispatch.child_budget_usage;
        if let (Some(registry), Some(extension_stores)) = (extension_registry, extension_stores) {
            registry.on_tool_finish(ToolFinishInput {
                thread_store: extension_stores.thread_store(),
                turn_store: extension_stores.turn_store(),
                tool_name: execution_request.name.as_str(),
                call_id: &execution_request.id,
                outcome: tool_call_outcome_for_result(&result),
            });
        }
        let mut completion = self.finish_tool_result(
            events,
            sink,
            execution_request,
            &result,
            hooks,
            &cwd_display,
            emit_deltas,
            provider_response_ingress,
            Some(cancel),
            child_budget_usage,
        );
        if !completion
            .event_error
            .as_ref()
            .is_some_and(is_semantic_commit_failure)
            && dispatch_event_error.is_some()
        {
            completion.event_error = dispatch_event_error;
        }
        completion.disposition = dispatch.disposition;
        Ok(completion)
    }

    pub(crate) fn handle_approval<W: io::Write>(
        &mut self,
        context: ToolApprovalGateContext<'_, W>,
    ) -> ApprovalGateExecution {
        let ToolApprovalGateContext {
            config,
            events,
            sink,
            tool_request,
            invocation,
            policy,
            permission_overlay,
            approval_handler,
            cancel,
            emit_deltas,
            provider_response_ingress,
        } = context;

        if tool_request.name == tool_types::ToolName::RequestPermissions {
            return ApprovalGateExecution::default();
        }

        if let Some(approval) = approval_request_for_invocation(invocation)
            && agent_common::requires_approval(approval.action)
        {
            let approval_decision = RuntimeToolApprovalPolicy::new(policy, permission_overlay)
                .resolve(approval.clone(), tool_request);
            let mut event_error = None;
            if emit_deltas {
                retain_first_io_error(
                    &mut event_error,
                    sink.emit(events.approval_requested(&approval)),
                );
            }

            match approval_decision {
                RuntimeApprovalDecision::Allowed(resolution) => {
                    if emit_deltas {
                        retain_first_io_error(
                            &mut event_error,
                            sink.emit(events.approval_resolved(&resolution)),
                        );
                    }
                    if event_error.is_some() {
                        return failed_approval_gate_before_start(
                            events,
                            sink,
                            tool_request,
                            emit_deltas,
                            provider_response_ingress,
                            event_error,
                            "tool dispatch stopped because approval events could not be delivered",
                        );
                    }
                }
                RuntimeApprovalDecision::Ask(approval) => {
                    if event_error.is_some() {
                        return failed_approval_gate_before_start(
                            events,
                            sink,
                            tool_request,
                            emit_deltas,
                            provider_response_ingress,
                            event_error,
                            "tool dispatch stopped before interactive approval because the request event could not be delivered",
                        );
                    }
                    let fallback_handler = RuntimeConfigApprovalHandler::new(config);
                    let handler: &dyn RuntimeApprovalHandler = approval_handler
                        .map(|handler| handler as &dyn RuntimeApprovalHandler)
                        .unwrap_or(&fallback_handler);
                    let final_resolution = match self.runtime.resolve_interactive_tool_approval(
                        handler,
                        &approval,
                        tool_request,
                    ) {
                        Ok(resolution) => resolution,
                        Err(error)
                            if error.kind() == io::ErrorKind::Interrupted
                                || cancel.is_cancelled() =>
                        {
                            let result = tool_types::ToolResult::cancelled(
                                tool_request,
                                "interactive approval was interrupted",
                                None,
                            );
                            return finish_approval_gate_terminal(
                                events,
                                sink,
                                tool_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                RunStatus::Cancelled,
                                result,
                            );
                        }
                        Err(error) => {
                            return failed_approval_gate_before_start(
                                events,
                                sink,
                                tool_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                &format!(
                                    "interactive approval failed before tool dispatch: {error}"
                                ),
                            );
                        }
                    };
                    if emit_deltas {
                        retain_first_io_error(
                            &mut event_error,
                            sink.emit(events.approval_resolved(&final_resolution)),
                        );
                    }
                    match final_resolution.decision {
                        ApprovalDecision::Deny => {
                            let result = tool_types::ToolResult::denied(
                                tool_request,
                                final_resolution.reason,
                            );
                            return finish_approval_gate_terminal(
                                events,
                                sink,
                                tool_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                RunStatus::ApprovalRequired,
                                result,
                            );
                        }
                        ApprovalDecision::Allow if event_error.is_none() => {}
                        ApprovalDecision::Allow => {
                            return failed_approval_gate_before_start(
                                events,
                                sink,
                                tool_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                "tool dispatch stopped because the approval resolution event could not be delivered",
                            );
                        }
                        ApprovalDecision::Ask => {
                            return failed_approval_gate_before_start(
                                events,
                                sink,
                                tool_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                "interactive approval did not resolve before tool dispatch",
                            );
                        }
                    }
                }
                RuntimeApprovalDecision::Denied { resolution, result } => {
                    if emit_deltas {
                        retain_first_io_error(
                            &mut event_error,
                            sink.emit(events.approval_resolved(&resolution)),
                        );
                    }
                    return finish_approval_gate_terminal(
                        events,
                        sink,
                        tool_request,
                        emit_deltas,
                        provider_response_ingress,
                        event_error,
                        RunStatus::ApprovalRequired,
                        result,
                    );
                }
                RuntimeApprovalDecision::NotRequired if event_error.is_none() => {}
                RuntimeApprovalDecision::NotRequired => {
                    return failed_approval_gate_before_start(
                        events,
                        sink,
                        tool_request,
                        emit_deltas,
                        provider_response_ingress,
                        event_error,
                        "tool dispatch stopped because approval events could not be delivered",
                    );
                }
            }
        }
        ApprovalGateExecution::default()
    }

    fn apply_pre_tool_hook(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<impl io::Write>,
        tool_request: &tool_types::ToolRequest,
        invocation: ToolInvocation,
        hooks: &HookRunner,
        cwd_display: &str,
        mcp_registry: &McpRegistry,
        emit_deltas: bool,
        provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
        cancel: Option<&CancelToken>,
    ) -> PreToolHookExecution {
        let mut event_error = None;
        let pre_tool_outcome =
            match self.run_pre_tool_hook(hooks, cwd_display, tool_request, cancel) {
                Ok(outcome) => outcome,
                Err(result) => {
                    retain_first_io_error(
                        &mut event_error,
                        publish_tool_call_completed(
                            events,
                            sink,
                            tool_request,
                            &result,
                            emit_deltas,
                            provider_response_ingress,
                        ),
                    );
                    let status = run_status_from_tool_status(result.status);
                    return PreToolHookExecution {
                        outcome: Err((status, result)),
                        event_error,
                    };
                }
            };
        match apply_pre_tool_outcome(invocation, &pre_tool_outcome, mcp_registry, config) {
            Ok(invocation) => PreToolHookExecution {
                outcome: Ok(invocation),
                event_error,
            },
            Err(error) => {
                let result = error.into_result();
                retain_first_io_error(
                    &mut event_error,
                    publish_tool_call_completed(
                        events,
                        sink,
                        tool_request,
                        &result,
                        emit_deltas,
                        provider_response_ingress,
                    ),
                );
                PreToolHookExecution {
                    outcome: Err((RunStatus::Failed, result)),
                    event_error,
                }
            }
        }
    }

    fn finish_tool_result(
        &mut self,
        events: &mut EventFactory,
        sink: &mut EventSink<impl io::Write>,
        execution_request: &tool_types::ToolRequest,
        result: &tool_types::ToolResult,
        hooks: &HookRunner,
        cwd_display: &str,
        emit_deltas: bool,
        provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
        cancel: Option<&CancelToken>,
        child_budget_usage: Option<orca_core::budget::BudgetUsage>,
    ) -> ToolExecutionCompletion {
        let mut event_error = None;
        retain_first_io_error(
            &mut event_error,
            publish_tool_call_completed(
                events,
                sink,
                execution_request,
                result,
                emit_deltas,
                provider_response_ingress,
            ),
        );
        if emit_deltas {
            if execution_request.name == tool_types::ToolName::UpdatePlan
                && result.status == tool_types::ToolStatus::Completed
            {
                match orca_tools::update_plan::parse_args(execution_request) {
                    Ok(update) => {
                        if let Some(ingress) = provider_response_ingress {
                            retain_first_io_error(
                                &mut event_error,
                                ingress.commit_plan_update(&update),
                            );
                        } else {
                            retain_first_io_error(
                                &mut event_error,
                                sink.emit(events.plan_updated(&update)),
                            );
                        }
                    }
                    Err(error) => retain_first_io_error(
                        &mut event_error,
                        sink.emit(events.error(&format!("failed to render plan update: {error}"))),
                    ),
                }
            }
            if let Some(warning) =
                self.run_post_tool_hook(hooks, cwd_display, execution_request, result, cancel)
            {
                retain_first_io_error(&mut event_error, sink.emit(events.error(&warning)));
            }
        }

        let status = run_status_from_tool_status(result.status);

        ToolExecutionCompletion {
            status,
            result: result.clone(),
            disposition: RuntimeToolTurnDisposition::ContinueModel,
            event_error,
            child_budget_usage,
        }
    }
}

fn failed_approval_gate_before_start<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
    event_error: Option<io::Error>,
    reason: &str,
) -> ApprovalGateExecution {
    let result = tool_types::ToolResult::failed_before_start(tool_request, reason, None);
    finish_approval_gate_terminal(
        events,
        sink,
        tool_request,
        emit_deltas,
        provider_response_ingress,
        event_error,
        RunStatus::Failed,
        result,
    )
}

fn finish_approval_gate_terminal<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
    mut event_error: Option<io::Error>,
    status: RunStatus,
    result: tool_types::ToolResult,
) -> ApprovalGateExecution {
    if emit_deltas {
        retain_first_io_error(
            &mut event_error,
            emit_tool_call_requested(events, sink, tool_request),
        );
    }
    retain_first_io_error(
        &mut event_error,
        publish_tool_call_completed(
            events,
            sink,
            tool_request,
            &result,
            emit_deltas,
            provider_response_ingress,
        ),
    );
    ApprovalGateExecution {
        outcome: Some((status, result)),
        event_error,
    }
}

fn retain_first_io_error(slot: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

fn tool_call_outcome_for_result(result: &tool_types::ToolResult) -> ToolCallOutcome {
    match result.status {
        tool_types::ToolStatus::Completed => ToolCallOutcome::Completed,
        tool_types::ToolStatus::Failed => ToolCallOutcome::Failed {
            started: result.terminal().started,
        },
        tool_types::ToolStatus::NotImplemented => ToolCallOutcome::Failed {
            started: tool_types::ToolInvocationStarted::No,
        },
        tool_types::ToolStatus::Denied => ToolCallOutcome::Blocked,
        tool_types::ToolStatus::Cancelled => ToolCallOutcome::Cancelled {
            started: result.terminal().started,
        },
        tool_types::ToolStatus::Indeterminate => ToolCallOutcome::Indeterminate {
            started: result.terminal().started,
        },
    }
}

fn emit_tool_call_requested(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    request: &tool_types::ToolRequest,
) -> io::Result<()> {
    let event = RuntimeTaskActor::tool_call_requested_event_for(events, request);
    sink.emit(event)
}

fn publish_tool_call_completed(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    request: &tool_types::ToolRequest,
    result: &tool_types::ToolResult,
    emit_deltas: bool,
    provider_response_ingress: Option<&dyn RuntimeProviderResponseIngress>,
) -> io::Result<()> {
    if let Some(ingress) = provider_response_ingress {
        ingress
            .commit_tool_result(result)
            .map_err(semantic_commit_failure)?;
    }
    if emit_deltas {
        let event = RuntimeTaskActor::tool_call_completed_event_for(events, request, result);
        sink.emit(event)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use orca_core::approval_rules::{PermissionRule, PermissionRules};
    use orca_core::approval_types::{
        ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest, Decision,
    };
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::hook_types::{HookConfig, HookEvent};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolStatus};
    use orca_mcp::McpRegistry;

    use super::{
        ToolApprovalGateContext, ToolExecutionActor, ToolExecutionContext,
        policy_for_tool_execution, publish_tool_call_completed, tool_call_outcome_for_result,
    };
    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::extension::{
        ExtensionData, ExtensionRegistryBuilder, RuntimeExtensionStores, ToolFinishInput,
        ToolLifecycleContributor, ToolStartInput,
    };
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::{
        RuntimeUserInputHandler, RuntimeUserInputRequest, TurnPermissionOverlay,
    };
    use crate::memory::MemoryBlock;
    use crate::model_response::RuntimeModelResponse;
    use crate::runtime_surface::RuntimeProviderResponseIngress;
    use crate::tasks::TaskRegistry;

    fn platform_hook_script(unix: impl Into<String>, windows: impl Into<String>) -> String {
        if cfg!(windows) {
            windows.into()
        } else {
            unix.into()
        }
    }

    #[derive(Debug)]
    struct RecordingSemanticIngress {
        committed: Arc<AtomicBool>,
    }

    impl RuntimeProviderResponseIngress for RecordingSemanticIngress {
        fn commit_response(&self, _response: &RuntimeModelResponse) -> io::Result<()> {
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
            self.committed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct DurableCommitAwareWriter {
        committed: Arc<AtomicBool>,
    }

    impl io::Write for DurableCommitAwareWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if !self.committed.load(Ordering::SeqCst) {
                return Err(io::Error::other(
                    "legacy tool terminal became visible before durable commit",
                ));
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn typed_tool_completion_commits_before_legacy_visibility() {
        let committed = Arc::new(AtomicBool::new(false));
        let ingress = RecordingSemanticIngress {
            committed: Arc::clone(&committed),
        };
        let mut events = EventFactory::new("typed-tool-terminal-order".to_string());
        let mut sink = EventSink::new(
            DurableCommitAwareWriter {
                committed: Arc::clone(&committed),
            },
            OutputFormat::Jsonl,
        );
        let request = ToolRequest {
            id: "tool-order-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf durable".to_string()),
            raw_arguments: Some(r#"{"command":"printf durable"}"#.to_string()),
        };
        let result = ToolResult::completed(&request, "durable".to_string(), false);

        publish_tool_call_completed(
            &mut events,
            &mut sink,
            &request,
            &result,
            true,
            Some(&ingress),
        )
        .unwrap();

        assert!(committed.load(Ordering::SeqCst));
    }

    fn config_with_permission_rules(permission_rules: PermissionRules) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules,
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("read_file tool execution must not execute child agents")
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
                return Err(io::Error::other("approval event consumer disconnected"));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailFirstFlush;

    impl io::Write for FailFirstFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("approval event consumer disconnected"))
        }
    }

    #[test]
    fn policy_for_tool_execution_preserves_config_permission_rules() {
        let config = config_with_permission_rules(PermissionRules {
            rules: vec![PermissionRule::new("bash", "rm *", Decision::Deny)],
        });
        let request = ApprovalRequest {
            id: "approval-tool-1".to_string(),
            action: ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("rm scratch.txt".to_string()),
            preview: None,
        };

        let policy = policy_for_tool_execution(&config);
        let resolution = policy.resolve_for_tool(&request, "bash", Some("rm scratch.txt"));

        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert!(resolution.reason.contains("permission deny rule"));
    }

    #[test]
    fn tool_terminal_status_maps_to_runtime_and_extension_lifecycle() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("sleep 30".to_string()),
            raw_arguments: None,
        };
        let hooks = HookRunner::default();
        let mut actor = ToolExecutionActor::new("tool-terminal-status");
        let mut events = EventFactory::new("tool-terminal-status".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);

        let cancelled = ToolResult::cancelled(&request, "turn interrupted", Some(130));
        let completion = actor.finish_tool_result(
            &mut events,
            &mut sink,
            &request,
            &cancelled,
            &hooks,
            ".",
            false,
            None,
            None,
            None,
        );
        assert_eq!(completion.status, RunStatus::Cancelled);
        assert!(completion.event_error.is_none());
        assert_eq!(
            tool_call_outcome_for_result(&cancelled),
            crate::extension::ToolCallOutcome::Cancelled {
                started: orca_core::tool_types::ToolInvocationStarted::Yes
            }
        );

        let indeterminate = ToolResult::indeterminate(&request, "missing terminal result");
        let completion = actor.finish_tool_result(
            &mut events,
            &mut sink,
            &request,
            &indeterminate,
            &hooks,
            ".",
            false,
            None,
            None,
            None,
        );
        assert_eq!(completion.status, RunStatus::Failed);
        assert!(completion.event_error.is_none());
        assert_eq!(
            tool_call_outcome_for_result(&indeterminate),
            crate::extension::ToolCallOutcome::Indeterminate {
                started: orca_core::tool_types::ToolInvocationStarted::Unknown
            }
        );
    }

    #[test]
    fn cancelled_pre_tool_hook_returns_cancelled_before_start_result() {
        let config = config_with_permission_rules(PermissionRules::default());
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo should-not-run".to_string()),
            raw_arguments: Some(r#"{"command":"echo should-not-run"}"#.to_string()),
        };
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "sleep 5".to_string(),
            tool: Some("bash".to_string()),
        }]);
        let mcp_registry = McpRegistry::default();
        let invocation =
            crate::tool_invocation::prepare_tool_invocation(&request, 0, &mcp_registry, &config);
        let cancel = orca_core::cancel::CancelToken::new();
        cancel.cancel();
        let mut actor = ToolExecutionActor::new("cancelled-pre-tool-hook");
        let mut events = EventFactory::new("cancelled-pre-tool-hook".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);

        let execution = actor.apply_pre_tool_hook(
            &config,
            &mut events,
            &mut sink,
            &request,
            invocation,
            &hooks,
            ".",
            &mcp_registry,
            false,
            None,
            Some(&cancel),
        );
        assert!(execution.event_error.is_none());

        let Err((status, result)) = execution.outcome else {
            panic!("cancelled pre-tool hook must stop before tool execution");
        };
        assert_eq!(status, RunStatus::Cancelled);
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.terminal().started,
            orca_core::tool_types::ToolInvocationStarted::No
        );
        assert_eq!(
            tool_call_outcome_for_result(&result),
            crate::extension::ToolCallOutcome::Cancelled {
                started: orca_core::tool_types::ToolInvocationStarted::No
            }
        );
    }

    #[test]
    fn approval_gate_consumes_matching_preapproved_tool_call_once() {
        let config = config_with_permission_rules(PermissionRules::default());
        let mut events = EventFactory::new("preapproved-tool".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = ToolRequest {
            id: "shell-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let registry = McpRegistry::default();
        let invocation =
            crate::tool_invocation::prepare_tool_invocation(&request, 0, &registry, &config);
        let policy = policy_for_tool_execution(&config);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("shell-1".to_string()));
        let mut actor = ToolExecutionActor::new(events.run_id().to_string());
        let cancel = orca_core::cancel::CancelToken::new();

        let execution = actor.handle_approval(ToolApprovalGateContext {
            config: &config,
            events: &mut events,
            sink: &mut sink,
            tool_request: &request,
            invocation: &invocation,
            policy: &policy,
            permission_overlay: &mut overlay,
            approval_handler: None,
            cancel: &cancel,
            emit_deltas: true,
            provider_response_ingress: None,
        });

        assert!(execution.outcome.is_none());
        assert!(execution.event_error.is_none());
        assert!(!overlay.consume_preapproved_tool_call_id("shell-1"));
    }

    #[test]
    fn denied_approval_preserves_terminal_after_resolved_event_io_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_permission_rules(PermissionRules {
            rules: vec![PermissionRule::new("bash", "rm *", Decision::Deny)],
        });
        config.approval_mode = ApprovalMode::FullAuto;
        let request = ToolRequest {
            id: "denied-shell".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("rm scratch.txt".to_string()),
            raw_arguments: Some(r#"{"command":"rm scratch.txt"}"#.to_string()),
        };
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("denied-approval-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let mut events = EventFactory::new("denied-approval-event-error".to_string());
        let mut sink = EventSink::new(FailSecondFlush::default(), OutputFormat::Text);
        let mut actor = ToolExecutionActor::new(events.run_id().to_string());

        let completion = actor
            .execute_with_event_error(
                &config,
                &mut events,
                &mut sink,
                &request,
                ToolExecutionContext::new(cwd.path(), 0, true, &policy)
                    .with_services(&instructions, &memory, &registry, &hooks)
                    .with_runtime(
                        &mut cost_tracker,
                        &cancel,
                        &task_registry,
                        &mut background_workflows,
                        None,
                    )
                    .with_permission_overlay(&mut permission_overlay),
                unused_child_executor,
                unused_child_executor,
            )
            .expect("known approval denial must survive event I/O failure");

        assert_eq!(completion.status, RunStatus::ApprovalRequired);
        assert_eq!(completion.result.status, ToolStatus::Denied);
        assert_eq!(
            completion.result.terminal().started,
            orca_core::tool_types::ToolInvocationStarted::No
        );
        assert!(
            completion
                .event_error
                .as_ref()
                .is_some_and(|error| error.to_string().contains("consumer disconnected"))
        );
    }

    #[test]
    fn approval_event_io_error_stops_dispatch_with_failed_before_start_terminal() {
        let cwd = tempfile::tempdir().expect("cwd");
        let marker = cwd.path().join("must-not-exist");
        let mut config = config_with_permission_rules(PermissionRules::default());
        config.approval_mode = ApprovalMode::FullAuto;
        let request = ToolRequest {
            id: "allowed-shell".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(format!("touch {}", marker.display())),
            raw_arguments: Some(
                serde_json::json!({ "command": format!("touch {}", marker.display()) }).to_string(),
            ),
        };
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("allowed-approval-event-error".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let mut events = EventFactory::new("allowed-approval-event-error".to_string());
        let mut sink = EventSink::new(FailFirstFlush, OutputFormat::Text);
        let mut actor = ToolExecutionActor::new(events.run_id().to_string());

        let completion = actor
            .execute_with_event_error(
                &config,
                &mut events,
                &mut sink,
                &request,
                ToolExecutionContext::new(cwd.path(), 0, true, &policy)
                    .with_services(&instructions, &memory, &registry, &hooks)
                    .with_runtime(
                        &mut cost_tracker,
                        &cancel,
                        &task_registry,
                        &mut background_workflows,
                        None,
                    )
                    .with_permission_overlay(&mut permission_overlay),
                unused_child_executor,
                unused_child_executor,
            )
            .expect("pre-dispatch event I/O failure must produce a tool terminal");

        assert_eq!(completion.status, RunStatus::Failed);
        assert_eq!(completion.result.status, ToolStatus::Failed);
        assert_eq!(
            completion.result.terminal().started,
            orca_core::tool_types::ToolInvocationStarted::No
        );
        assert!(completion.event_error.is_some());
        assert!(!marker.exists());
    }

    #[test]
    fn tool_execution_notifies_extension_lifecycle_for_completed_tool() {
        #[derive(Debug)]
        struct RecordingContributor {
            events: Arc<Mutex<Vec<String>>>,
        }

        impl ToolLifecycleContributor for RecordingContributor {
            fn on_tool_start(&self, input: ToolStartInput<'_>) {
                self.events.lock().unwrap().push(format!(
                    "start:{}:{}:{}",
                    input.thread_store.level_id(),
                    input.turn_store.level_id(),
                    input.call_id
                ));
            }

            fn on_tool_finish(&self, input: ToolFinishInput<'_>) {
                self.events.lock().unwrap().push(format!(
                    "finish:{}:{}:{}:{:?}",
                    input.thread_store.level_id(),
                    input.turn_store.level_id(),
                    input.call_id,
                    input.outcome
                ));
            }
        }

        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        let mut config = config_with_permission_rules(PermissionRules::default());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("extension-tool-execution".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = ToolRequest {
            id: "read-file".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("extension-tool-execution".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let lifecycle_events = Arc::new(Mutex::new(Vec::new()));
        let mut extension_builder = ExtensionRegistryBuilder::new();
        extension_builder.tool_lifecycle_contributor(Arc::new(RecordingContributor {
            events: Arc::clone(&lifecycle_events),
        }));
        let extension_registry = Arc::new(extension_builder.build());
        let thread_store = Arc::new(ExtensionData::new("thread-1"));
        let turn_store = Arc::new(ExtensionData::new("turn-1"));
        let context = ToolExecutionContext::new(cwd.path(), 0, true, &policy)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_runtime(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
            )
            .with_permission_overlay(&mut permission_overlay)
            .with_extensions(
                &extension_registry,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
            );

        let mut actor = ToolExecutionActor::new(events.run_id().to_string());
        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                context,
                unused_child_executor,
                unused_child_executor,
            )
            .expect("execute read_file");

        assert_eq!(status, RunStatus::Success);
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            lifecycle_events.lock().unwrap().as_slice(),
            [
                "start:thread-1:turn-1:read-file",
                "finish:thread-1:turn-1:read-file:Completed"
            ]
        );
    }

    #[test]
    fn approval_relevant_pre_tool_mutation_fails_before_dispatch() {
        let cwd = tempfile::tempdir().expect("cwd");
        let marker = cwd.path().join("mutated-effect");
        let mut config = config_with_permission_rules(PermissionRules::default());
        config.approval_mode = ApprovalMode::FullAuto;
        let request = ToolRequest {
            id: "approved-effect".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("true".to_string()),
            raw_arguments: Some(r#"{"command":"true"}"#.to_string()),
        };
        let hook_output = serde_json::json!({
            "action": "modify",
            "modified_target": format!("touch {}", marker.display()),
        })
        .to_string();
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: platform_hook_script(
                format!("printf '%s' '{}'", hook_output),
                format!(
                    "[Console]::Out.Write('{}')",
                    hook_output.replace('\'', "''")
                ),
            ),
            tool: Some("bash".to_string()),
        }]);
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("hook-mutation-authority".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let context = ToolExecutionContext::new(cwd.path(), 0, false, &policy)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_runtime(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
            )
            .with_permission_overlay(&mut permission_overlay);
        let mut events = EventFactory::new("hook-mutation-authority".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut actor = ToolExecutionActor::new(events.run_id().to_string());

        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                context,
                unused_child_executor,
                unused_child_executor,
            )
            .expect("mutation must fail as a normal tool terminal");

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(
            result.terminal().started,
            orca_core::tool_types::ToolInvocationStarted::No
        );
        assert!(!marker.exists(), "mutated effect must never reach dispatch");
    }

    #[test]
    fn tool_execution_routes_ask_user_question_through_turn_interaction_handler() {
        struct AnswerHandler;

        impl RuntimeUserInputHandler for AnswerHandler {
            fn request_user_input(
                &self,
                request: &RuntimeUserInputRequest,
            ) -> io::Result<Option<String>> {
                assert_eq!(request.id, "ask:question:1");
                assert_eq!(request.question, "Confirm: Continue?");
                assert_eq!(
                    request.choices,
                    vec!["yes - Continue".to_string(), "no - Stop".to_string()]
                );
                Ok(Some("yes".to_string()))
            }
        }

        let config = config_with_permission_rules(PermissionRules::default());
        let mut actor = ToolExecutionActor::new("tool-user-input-run");
        let mut events = EventFactory::new("tool-user-input-run".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = std::env::current_dir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::new(Vec::new());
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new_for_cwd("tool-user-input-run".to_string(), &cwd);
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let handler = AnswerHandler;
        let request = ToolRequest {
            id: "ask".to_string(),
            name: ToolName::AskUserQuestion,
            action: ActionKind::Read,
            target: Some("Continue?".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "questions": [{
                        "header": "Confirm",
                        "question": "Continue?",
                        "options": [
                            {"label": "yes", "description": "Continue"},
                            {"label": "no", "description": "Stop"}
                        ]
                    }]
                })
                .to_string(),
            ),
        };

        let (_status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                ToolExecutionContext::new(&cwd, 0, true, &policy_for_tool_execution(&config))
                    .with_services(&instructions, &memory, &mcp_registry, &hooks)
                    .with_runtime(
                        &mut cost_tracker,
                        &cancel,
                        &task_registry,
                        &mut background_workflows,
                        None,
                    )
                    .with_permission_overlay(&mut permission_overlay)
                    .with_user_input_handler(Some(&handler)),
                unused_child_executor,
                unused_child_executor,
            )
            .expect("tool execution");

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some(r#"{"answers":{"Continue?":"yes"}}"#)
        );
    }

    #[test]
    fn tool_execution_maps_interaction_io_error_to_indeterminate_terminal() {
        struct ErrorHandler;

        impl RuntimeUserInputHandler for ErrorHandler {
            fn request_user_input(
                &self,
                _request: &RuntimeUserInputRequest,
            ) -> io::Result<Option<String>> {
                Err(io::Error::other("interaction channel closed"))
            }
        }

        let config = config_with_permission_rules(PermissionRules::default());
        let mut actor = ToolExecutionActor::new("tool-user-input-error");
        let mut events = EventFactory::new("tool-user-input-error".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let cwd = std::env::current_dir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new_for_cwd("tool-user-input-error".to_string(), &cwd);
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let request = ToolRequest {
            id: "ask-error".to_string(),
            name: ToolName::AskUserQuestion,
            action: ActionKind::Read,
            target: Some("Continue?".to_string()),
            raw_arguments: Some(
                r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#
                    .to_string(),
            ),
        };

        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                ToolExecutionContext::new(&cwd, 0, true, &policy_for_tool_execution(&config))
                    .with_services(&instructions, &memory, &mcp_registry, &hooks)
                    .with_runtime(
                        &mut cost_tracker,
                        &cancel,
                        &task_registry,
                        &mut background_workflows,
                        None,
                    )
                    .with_permission_overlay(&mut permission_overlay)
                    .with_user_input_handler(Some(&ErrorHandler)),
                unused_child_executor,
                unused_child_executor,
            )
            .expect("interaction I/O error must still produce a tool terminal");

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(result.status, ToolStatus::Indeterminate);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("interaction channel closed"))
        );
    }
}
