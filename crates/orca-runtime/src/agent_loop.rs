use std::io;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
use crate::cost::CostTracker;
use crate::lifecycle::{
    AgentLoopContext, AgentLoopOutcome, RuntimeSessionLifecycle, RuntimeTaskActor,
    RuntimeTurnContext, RuntimeTurnExecution,
};
use crate::runtime_conversation_bootstrap::{
    AgentConversationContext, RuntimeConversationBootstrapStep,
};
use crate::runtime_turn_loop::{
    RuntimeAgentTurnLoopInput, RuntimeTurnLoopExecutors, RuntimeTurnLoopStep,
    RuntimeTurnOutputContext, RuntimeTurnPolicyContext, RuntimeTurnProviderContext,
    RuntimeTurnRequestContext, RuntimeTurnWorkflowContext, run_agent_turn_loop,
};
use crate::runtime_turn_setup::RuntimeTurnSetupStep;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow_execution::observe_background_workflows;
use orca_core::budget::OperationTerminal;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

#[cfg(test)]
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeTurnInteractionState, RuntimeTurnState,
    RuntimeUserInputHandler, RuntimeUserInputRequest,
};

pub(crate) fn run_agent_loop(
    config: &RunConfig,
    loop_context: AgentLoopContext<'_>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    conversation_context: AgentConversationContext<'_>,
    tool_policy: AgentToolPolicyContext<'_>,
) -> io::Result<AgentLoopOutcome> {
    let AgentLoopContext {
        turn_context,
        turn_deps,
        turn_state,
        turn_execution,
    } = loop_context;
    let RuntimeTurnContext {
        turn_id,
        cwd,
        prompt,
        subagent_depth,
        subagent_type,
        ..
    } = turn_context.clone();
    let turn_deps = turn_deps.expect("agent loop turn deps");
    let turn_state = turn_state.expect("agent loop turn state");
    let loop_state = turn_state.into_loop_state();
    let RuntimeTurnExecution {
        background_workflows,
        workflow_ipc,
        lifecycle,
    } = turn_execution.expect("agent loop turn execution");
    // One OperationContext per agent loop (root and each child) owns the
    // BudgetController and the ExecutionJournal for this operation. Any
    // history mode backed by a durable session (Record, Resume, ResumeAt,
    // Fork) persists the journal under ORCA_HOME; only Disabled operations
    // journal to the temp directory so stateless runs never create runtime
    // artifacts in ORCA_HOME.
    let mut operation = crate::operation_context::OperationContext::open(
        config.budget.to_spec(),
        turn_id.as_str(),
        !matches!(
            config.history_mode,
            orca_core::config::HistoryMode::Disabled
        ),
    )?;
    let setup = RuntimeTurnSetupStep::new().prepare(
        config,
        subagent_depth,
        subagent_type,
        loop_state.tool_policy(tool_policy),
        turn_deps.mcp_registry,
    );
    let ctx_config = setup.context_config;
    let policy = setup.policy;
    let provider_config = setup.provider_config;

    let mut prepared_conversation = RuntimeConversationBootstrapStep::new().prepare(
        conversation_context,
        cwd,
        prompt,
        subagent_depth,
        subagent_type,
        turn_deps.instructions,
        config.approval_mode,
        turn_deps.memory,
    );

    let mut legacy_lifecycle = RuntimeSessionLifecycle::new(events.run_id().to_string());
    let lifecycle = lifecycle.unwrap_or(&mut legacy_lifecycle);
    let mut actor = RuntimeTaskActor::new(lifecycle);
    let mut turn_loop_step = RuntimeTurnLoopStep::new();

    let mut outcome = run_agent_turn_loop(
        &mut turn_loop_step,
        RuntimeAgentTurnLoopInput {
            actor: &mut actor,
            operation: &mut operation,
            provider_context: RuntimeTurnProviderContext::new(
                config.provider,
                &ctx_config,
                &provider_config,
                &config.model,
            ),
            request: RuntimeTurnRequestContext::new(turn_context),
            deps: turn_deps,
            output: RuntimeTurnOutputContext::new(events, sink),
            prepared_conversation: &mut prepared_conversation,
            loop_state,
            policy: RuntimeTurnPolicyContext::new(config, tool_policy, &policy),
            workflow: RuntimeTurnWorkflowContext::new(background_workflows, workflow_ipc),
        },
        RuntimeTurnLoopExecutors::new(execute_child_agent_loop, execute_child_agent_loop),
    )?;

    // The loop always ends with a typed terminal: budget stops already
    // committed `checkpoint.created` + `operation.terminal` durably before
    // surfacing, so every other exit appends the terminal once here. The
    // journal is the source of truth for the operation's terminal fact, and
    // the `Completed` usage is finalized from the controller before commit.
    // ApprovalRequired is NOT a terminal: the operation is parked waiting
    // for approval and may resume, so no terminal is committed for it.
    if let AgentLoopOutcome::Completed(result) = &mut outcome {
        if result.status == orca_core::event_schema::RunStatus::ApprovalRequired {
            return Ok(outcome);
        }
        if let OperationTerminal::Completed { usage } = &mut result.terminal {
            *usage = operation.controller.usage();
        }
        operation.commit_terminal(turn_id.as_str(), result.terminal.clone())?;
    }

    Ok(outcome)
}

pub(crate) fn execute_child_agent_loop<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
    child_cost_tracker: &mut CostTracker,
) -> io::Result<ChildAgentResult> {
    let fallback_task_registry;
    let task_registry = if let Some(task_registry) = runtime.task_registry {
        task_registry
    } else {
        fallback_task_registry =
            TaskRegistry::new_for_cwd(runtime.events.run_id().to_string(), runtime.cwd);
        &fallback_task_registry
    };
    let mut background_workflows = Vec::new();
    let child = run_agent_loop(
        config,
        AgentLoopContext::new(
            runtime.cwd,
            &request.prompt,
            request.depth,
            request.emit_deltas,
            &request.subagent_type,
        )
        .with_root_task_id(runtime.root_task_id)
        .with_services(
            runtime.instructions,
            runtime.memory,
            runtime.mcp_registry,
            runtime.hooks,
        )
        .with_runtime(child_cost_tracker, runtime.cancel, task_registry)
        .with_execution(
            &mut background_workflows,
            request.workflow_ipc.as_ref(),
            runtime.lifecycle.as_deref_mut(),
        ),
        runtime.events,
        runtime.sink,
        AgentConversationContext::owned(),
        AgentToolPolicyContext::new(
            request.allowed_tools.as_deref(),
            request.tool_policy_label.as_deref(),
        ),
    )?;
    let AgentLoopOutcome::Completed(child) = child else {
        return Err(io::Error::other(
            "child agent provider suspension is not supported",
        ));
    };
    observe_background_workflows(
        config.output_format == OutputFormat::Jsonl,
        runtime.events,
        runtime.sink,
        &mut background_workflows,
        task_registry,
        runtime.cancel,
        None,
    )?;
    Ok(ChildAgentResult {
        status: child.status,
        final_message: child.final_message,
        error: child.error,
        // The child's consumed budget rides on the typed terminal usage so
        // the parent can merge the exact receipt into its own operation.
        budget_usage: child.terminal.usage(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::{RuntimeTaskKind, RuntimeTurnDeps};
    use crate::memory::MemoryBlock;
    use orca_core::cancel::CancelToken;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_types::SubagentType;
    use orca_mcp::McpRegistry;

    #[test]
    fn runtime_turn_context_snapshots_agent_loop_entry_values() {
        let cwd = std::env::temp_dir().join("orca-runtime-turn-context");
        let subagent_type = SubagentType::General;

        let context = RuntimeTurnContext::new(&cwd, "inspect repo", 2, false, &subagent_type);

        assert_eq!(context.cwd(), cwd.as_path());
        assert_eq!(context.prompt(), "inspect repo");
        assert_eq!(context.subagent_depth(), 2);
        assert!(!context.emit_deltas());
        assert_eq!(context.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_context() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-context");
        let subagent_type = SubagentType::General;

        let agent_context = AgentLoopContext::new(&cwd, "inspect repo", 1, true, &subagent_type);

        let context = agent_context.turn_context();
        assert_eq!(context.cwd(), cwd.as_path());
        assert_eq!(context.prompt(), "inspect repo");
        assert_eq!(context.subagent_depth(), 1);
        assert!(context.emit_deltas());
        assert_eq!(context.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_carries_initial_provider_response() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-continuation");
        let subagent_type = SubagentType::General;
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("continued".to_string())],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };

        let context = AgentLoopContext::new(&cwd, "inspect repo", 1, true, &subagent_type)
            .with_initial_response(response);

        assert_eq!(
            context
                .initial_response()
                .as_ref()
                .and_then(|response| response.assistant_content.as_deref()),
            Some("continued")
        );
    }

    #[test]
    fn agent_loop_context_carries_runtime_turn_continuation() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-runtime-continuation");
        let subagent_type = SubagentType::General;
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

        let context = AgentLoopContext::new(&cwd, "inspect repo", 1, true, &subagent_type)
            .with_turn_continuation(continuation);

        assert_eq!(
            context
                .continuation()
                .and_then(|continuation| continuation.preapproved_tool_call_id()),
            Some("tool-1")
        );
    }

    #[test]
    fn agent_loop_context_carries_readonly_services() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-services");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(context.instructions(), &instructions));
        assert!(std::ptr::eq(context.memory(), &memory));
        assert!(std::ptr::eq(context.mcp_registry(), &registry));
        assert!(std::ptr::eq(context.hooks(), &hooks));
    }

    #[test]
    fn runtime_turn_deps_group_agent_loop_services() {
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let deps = RuntimeTurnDeps::new(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_deps() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-deps");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        let deps = context.turn_deps();
        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_carries_runtime_refs() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-runtime");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-runtime".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(context.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(context.cancel(), &cancel));
        assert!(std::ptr::eq(context.task_registry(), &task_registry));
    }

    #[test]
    fn runtime_turn_state_groups_mutable_runtime_refs() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state".to_string());

        let state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn runtime_turn_state_exposes_runtime_extension_context() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-extension-context".to_string());
        let state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        let extensions = state.extension_context();
        let stores = extensions.stores();

        assert!(std::ptr::eq(
            extensions.registry(),
            state.extension_registry()
        ));
        assert!(std::ptr::eq(
            stores.thread_store(),
            state.thread_extensions()
        ));
        assert!(std::ptr::eq(stores.turn_store(), state.turn_extensions()));
    }

    #[test]
    fn runtime_turn_state_applies_runtime_directives_in_order() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-directives".to_string());
        let mut state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        state.apply_directive(crate::runtime_directive::RuntimeDirective::SwitchModel {
            model: orca_core::model::FLASH_MODEL.to_string(),
            reason: "skill requested cheaper execution".to_string(),
        });
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::ReplaceAllowedTools {
                tool_names: vec!["read_file".to_string(), "grep".to_string()],
                reason: "skill narrowed tool surface".to_string(),
            },
        );
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::InjectSystemMessage {
                message: "Prefer focused repository evidence.".to_string(),
                reason: "skill added runtime instruction".to_string(),
            },
        );

        let directives = &state.directive_state;
        assert_eq!(
            directives.model_override(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(
            directives.allowed_tools(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert_eq!(
            directives.pending_system_messages(),
            &["Prefer focused repository evidence.".to_string()]
        );
        assert_eq!(
            directives.transition_reasons(),
            &[
                "switch_model: skill requested cheaper execution".to_string(),
                "replace_allowed_tools: skill narrowed tool surface".to_string(),
                "inject_system_message: skill added runtime instruction".to_string(),
            ]
        );
    }

    #[test]
    fn runtime_directives_replace_agent_loop_tool_policy() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-tool-directives".to_string());
        let mut state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);
        state.apply_directive(
            crate::runtime_directive::RuntimeDirective::ReplaceAllowedTools {
                tool_names: vec!["read_file".to_string()],
                reason: "narrow current turn".to_string(),
            },
        );

        let loop_state = state.into_loop_state();
        let policy = loop_state.tool_policy(AgentToolPolicyContext::unrestricted());

        assert_eq!(
            policy.allowed_tools().unwrap(),
            &["read_file".to_string()][..]
        );
        assert_eq!(policy.label(), Some("runtime directive tool policy"));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_state() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-state");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state-context".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        let state = context.turn_state();
        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn agent_loop_context_carries_execution_refs() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-execution");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(context.background_workflow_count(), 0);
        assert!(context.workflow_ipc().is_none());
        assert_eq!(
            context.lifecycle().unwrap().run_id(),
            "agent-loop-execution"
        );
    }

    #[test]
    fn runtime_turn_execution_groups_lifecycle_refs() {
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-group");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let execution =
            RuntimeTurnExecution::new(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-group"
        );
    }

    struct TestPermissionHandler;
    struct TestUserInputHandler;

    impl RuntimePermissionRequestHandler for TestPermissionHandler {
        fn request_permissions(
            &self,
            _request: &crate::lifecycle::RuntimePermissionRequest,
        ) -> io::Result<crate::lifecycle::RuntimePermissionResponse> {
            unreachable!("test only checks handler routing identity")
        }
    }

    impl RuntimeUserInputHandler for TestUserInputHandler {
        fn request_user_input(
            &self,
            _request: &RuntimeUserInputRequest,
        ) -> io::Result<Option<String>> {
            unreachable!("test only checks handler routing identity")
        }
    }

    #[test]
    fn runtime_turn_interaction_state_groups_permission_handler() {
        let handler = TestPermissionHandler;
        let interactions =
            RuntimeTurnInteractionState::new().with_permission_handler(Some(&handler));

        let resolved = interactions
            .permission_handler()
            .expect("permission handler");
        let expected: &(dyn RuntimePermissionRequestHandler + Send + Sync) = &handler;
        assert!(std::ptr::eq(resolved, expected));
    }

    #[test]
    fn runtime_turn_interaction_state_groups_user_input_handler() {
        let handler = TestUserInputHandler;
        let interactions =
            RuntimeTurnInteractionState::new().with_user_input_handler(Some(&handler));

        let resolved = interactions
            .user_input_handler()
            .expect("user input handler");
        let expected: &dyn RuntimeUserInputHandler = &handler;
        assert!(std::ptr::eq(resolved, expected));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_interactions() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-interactions");
        let subagent_type = SubagentType::General;
        let handler = TestPermissionHandler;
        let user_input_handler = TestUserInputHandler;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_permission_handler(Some(&handler))
            .with_user_input_handler(Some(&user_input_handler));

        let resolved = context
            .turn_interactions()
            .permission_handler()
            .expect("permission handler");
        let expected: &(dyn RuntimePermissionRequestHandler + Send + Sync) = &handler;
        assert!(std::ptr::eq(resolved, expected));

        let resolved = context
            .turn_interactions()
            .user_input_handler()
            .expect("user input handler");
        let expected: &dyn RuntimeUserInputHandler = &user_input_handler;
        assert!(std::ptr::eq(resolved, expected));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_execution() {
        let cwd = std::env::temp_dir().join("orca-agent-loop-execution-context");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-context");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        let execution = context.turn_execution();
        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-context"
        );
    }
}
