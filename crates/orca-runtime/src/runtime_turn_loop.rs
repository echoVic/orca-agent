use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::lifecycle::{
    AgentLoopOutcome, RuntimeTaskActor, RuntimeTurnContext, RuntimeTurnDeps, RuntimeTurnLoopState,
};
use crate::operation_context::OperationContext;
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_iteration::{
    RuntimeTurnIterationInput, RuntimeTurnIterationResult, RuntimeTurnIterationStep,
};
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnLoopStep {
    iteration_step: RuntimeTurnIterationStep,
}

pub(crate) struct RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'ipc WorkflowIpcContext>,
}

pub(crate) struct RuntimeTurnOutputContext<'events, 'sink, W: io::Write> {
    pub(crate) events: &'events mut EventFactory,
    pub(crate) sink: &'sink mut EventSink<W>,
}

pub(crate) struct RuntimeTurnProviderContext<'a> {
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) model: &'a ModelSelection,
}

pub(crate) struct RuntimeTurnRequestContext<'a> {
    pub(crate) turn_context: RuntimeTurnContext<'a>,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeTurnPolicyContext<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) approval_policy: &'a ApprovalPolicy,
}

pub(crate) struct RuntimeAgentTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) policy: RuntimeTurnPolicyContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) struct RuntimeTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) policy: RuntimeTurnPolicyContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) struct RuntimeTurnLoopExecutors {
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput<'a, 'runtime, W> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: &'a mut RuntimeTaskActor<'runtime>,
        operation: &'a mut OperationContext,
        provider_context: RuntimeTurnProviderContext<'a>,
        request: RuntimeTurnRequestContext<'a>,
        deps: RuntimeTurnDeps<'a>,
        output: RuntimeTurnOutputContext<'a, 'a, W>,
        prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
        loop_state: RuntimeTurnLoopState<'a>,
        policy: RuntimeTurnPolicyContext<'a>,
        workflow: RuntimeTurnWorkflowContext<'a, 'a>,
    ) -> Self {
        Self {
            actor,
            operation,
            provider_context,
            request,
            deps,
            output,
            prepared_conversation,
            loop_state,
            policy,
            workflow,
        }
    }

    pub(crate) fn iteration_input<'iter>(
        &'iter mut self,
    ) -> RuntimeTurnIterationInput<'iter, 'runtime, W> {
        let loop_state = self.loop_state.iteration_state(self.policy.tool_policy);
        let policy = RuntimeTurnPolicyContext::new(
            self.policy.config,
            loop_state.tool_policy,
            self.policy.approval_policy,
        );
        RuntimeTurnIterationInput {
            actor: &mut *self.actor,
            operation: &mut *self.operation,
            provider_context: RuntimeTurnProviderContext::new(
                self.provider_context.provider,
                self.provider_context.context_config,
                self.provider_context.provider_config,
                self.provider_context.model,
            ),
            request: self.request.for_iteration(),
            deps: self.deps,
            output: RuntimeTurnOutputContext::new(&mut *self.output.events, &mut *self.output.sink),
            prepared_conversation: &mut *self.prepared_conversation,
            loop_state,
            policy,
            workflow: RuntimeTurnWorkflowContext::new(
                &mut *self.workflow.background_workflows,
                self.workflow.workflow_ipc,
            ),
        }
    }
}

impl<'background, 'ipc> RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) fn new(
        background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'ipc WorkflowIpcContext>,
    ) -> Self {
        Self {
            background_workflows,
            workflow_ipc,
        }
    }
}

impl<'events, 'sink, W: io::Write> RuntimeTurnOutputContext<'events, 'sink, W> {
    pub(crate) fn new(events: &'events mut EventFactory, sink: &'sink mut EventSink<W>) -> Self {
        Self { events, sink }
    }
}

impl<'a> RuntimeTurnProviderContext<'a> {
    pub(crate) fn new(
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        model: &'a ModelSelection,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            model,
        }
    }
}

impl<'a> RuntimeTurnRequestContext<'a> {
    pub(crate) fn new(turn_context: RuntimeTurnContext<'a>) -> Self {
        Self { turn_context }
    }

    fn for_iteration(&mut self) -> Self {
        let turn_context = RuntimeTurnContext {
            turn_id: self.turn_context.turn_id.clone(),
            cwd: self.turn_context.cwd,
            prompt: self.turn_context.prompt,
            subagent_depth: self.turn_context.subagent_depth,
            emit_deltas: self.turn_context.emit_deltas,
            subagent_type: self.turn_context.subagent_type,
            root_task_id: self.turn_context.root_task_id,
            continuation: self.turn_context.continuation.take(),
            steer_handle: self.turn_context.steer_handle,
            provider_suspension_control: self.turn_context.provider_suspension_control,
            provider_response_ingress: self.turn_context.provider_response_ingress,
            workflow_lifecycle_ingress: self.turn_context.workflow_lifecycle_ingress,
            wait_for_background_workflows: self.turn_context.wait_for_background_workflows,
            defer_cancel_terminal: self.turn_context.defer_cancel_terminal,
        };
        Self { turn_context }
    }
}

impl<'a> RuntimeTurnPolicyContext<'a> {
    pub(crate) fn new(
        config: &'a RunConfig,
        tool_policy: AgentToolPolicyContext<'a>,
        approval_policy: &'a ApprovalPolicy,
    ) -> Self {
        Self {
            config,
            tool_policy,
            approval_policy,
        }
    }
}

impl RuntimeTurnLoopExecutors {
    pub(crate) fn new(
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> Self {
        Self {
            workflow_child_executor,
            batch_child_executor,
        }
    }
}

pub(crate) fn run_agent_turn_loop<W: io::Write>(
    step: &mut RuntimeTurnLoopStep,
    input: RuntimeAgentTurnLoopInput<'_, '_, W>,
    executors: RuntimeTurnLoopExecutors,
) -> io::Result<AgentLoopOutcome> {
    step.run(input.into_turn_loop_input(), executors)
}

impl RuntimeTurnLoopStep {
    pub(crate) fn new() -> Self {
        Self {
            iteration_step: RuntimeTurnIterationStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnLoopInput<'_, '_, W>,
        executors: RuntimeTurnLoopExecutors,
    ) -> io::Result<AgentLoopOutcome> {
        loop {
            match self.iteration_step.run(
                input.iteration_input(),
                executors.workflow_child_executor,
                executors.batch_child_executor,
            )? {
                RuntimeTurnIterationResult::ContinueLoop => {
                    continue;
                }
                RuntimeTurnIterationResult::Return(result) => {
                    return Ok(AgentLoopOutcome::Completed(result));
                }
                RuntimeTurnIterationResult::Suspended(suspension) => {
                    return Ok(AgentLoopOutcome::ProviderSuspended(suspension));
                }
            }
        }
    }
}

impl<'a, 'runtime, W: io::Write> RuntimeAgentTurnLoopInput<'a, 'runtime, W> {
    fn into_turn_loop_input(self) -> RuntimeTurnLoopInput<'a, 'runtime, W> {
        RuntimeTurnLoopInput::new(
            self.actor,
            self.operation,
            self.provider_context,
            self.request,
            self.deps,
            self.output,
            self.prepared_conversation,
            self.loop_state,
            self.policy,
            self.workflow,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Conversation;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::subagent_types::SubagentType;
    use orca_mcp::McpRegistry;

    use crate::cost::CostTracker;
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTurnDeps, RuntimeTurnState};
    use crate::memory::MemoryBlock;
    use crate::runtime_conversation_bootstrap::{
        AgentConversationContext, RuntimeConversationBootstrapStep,
    };
    use crate::tasks::TaskRegistry;
    use crate::tool_execution::policy_for_tool_execution;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: orca_core::config::BudgetConfig::default(),
            mcp_servers: Vec::<McpServerConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
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

    #[test]
    fn turn_loop_input_passes_continuation_to_first_iteration_only() {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &config.model_runtime,
        );
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::PRO_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = policy_for_tool_execution(&config);
        let subagent_type = SubagentType::General;
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("turn-loop-continuation".to_string());
        let mut cost_tracker = CostTracker::new(None);
        let loop_state =
            RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry).into_loop_state();
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-loop-continuation");
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("turn-loop-continuation".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let mut prepared_conversation = RuntimeConversationBootstrapStep::new().prepare(
            AgentConversationContext::borrowed(&mut conversation, None),
            cwd.path(),
            "continue",
            0,
            &subagent_type,
            &instructions,
            config.approval_mode,
            &memory,
        );
        let mut background_workflows = Vec::new();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("continued".to_string())],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let continuation = crate::background_turn::RuntimeTurnContinuation {
            response: crate::model_response::RuntimeModelResponse::new(
                response,
                orca_core::thread_identity::TurnId::new(),
            ),
            preapproved_tool_call_id: Some("tool-1".to_string()),
        };
        let mut input = RuntimeTurnLoopInput {
            actor: &mut actor,
            operation: &mut crate::operation_context::OperationContext::for_tests(
                orca_core::budget::BudgetSpec::default(),
                "turn-loop-input-test",
            ),
            provider_context: RuntimeTurnProviderContext::new(
                ProviderKind::DeepSeek,
                &context_config,
                &provider_config,
                &config.model,
            ),
            request: RuntimeTurnRequestContext::new(
                RuntimeTurnContext::new(cwd.path(), "continue", 0, true, &subagent_type)
                    .with_continuation(continuation),
            ),
            deps: RuntimeTurnDeps::new(&instructions, &memory, &mcp_registry, &hooks),
            output: RuntimeTurnOutputContext::new(&mut events, &mut sink),
            prepared_conversation: &mut prepared_conversation,
            loop_state,
            policy: RuntimeTurnPolicyContext::new(
                &config,
                AgentToolPolicyContext::unrestricted(),
                &policy,
            ),
            workflow: RuntimeTurnWorkflowContext::new(&mut background_workflows, None),
        };

        {
            let first_iteration = input.iteration_input();
            assert_eq!(
                first_iteration
                    .request
                    .turn_context
                    .continuation
                    .as_ref()
                    .and_then(|continuation| {
                        continuation.response.response.assistant_content.as_deref()
                    }),
                Some("continued")
            );
            assert_eq!(
                first_iteration
                    .request
                    .turn_context
                    .continuation
                    .as_ref()
                    .and_then(|continuation| continuation.preapproved_tool_call_id()),
                Some("tool-1")
            );
        }
        {
            let second_iteration = input.iteration_input();
            assert!(second_iteration.request.turn_context.continuation.is_none());
        }
    }
}
