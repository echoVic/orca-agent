use std::cell::RefCell;
use std::io;
use std::io::Cursor;

use crate::agent_child::*;
use crate::cost::CostTracker;
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::{ActionKind, ApprovalMode};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::conversation::{Message, RawToolCall};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::external_config::ExternalToolConfig;
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::mcp_types::McpServerConfig;
use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};
use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
use orca_core::subagent_config::SubagentConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolInvocationStarted, ToolName, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use crate::child_agent_response_folding::fold_child_agent_tool_result_and_close_siblings;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

fn config(model: Option<&str>) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(model.map(str::to_string)),
        model_runtime: Default::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::<McpServerConfig>::new(),
        hooks: Vec::<HookConfig>::new(),
        external_tools: Vec::<ExternalToolConfig>::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: Default::default(),
        runtime_workspace_roots: None,
        permission_rules: PermissionRules::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}

fn runtime<'a>(
    sink: &'a mut EventSink<Cursor<Vec<u8>>>,
    events: &'a mut EventFactory,
    cancel: &'a CancelToken,
    executor: ChildAgentExecutor<Cursor<Vec<u8>>>,
) -> ChildAgentRuntime<'a, Cursor<Vec<u8>>> {
    let instructions = Box::leak(Box::new(ProjectInstructions::default()));
    let memory = Box::leak(Box::new(MemoryBlock::default()));
    let mcp_registry = Box::leak(Box::new(McpRegistry::default()));
    let hooks = Box::leak(Box::new(HookRunner::new(Vec::new())));
    let cwd = Box::leak(Box::new(std::env::temp_dir()));
    ChildAgentRuntime::new(ChildAgentRuntimeContext {
        cwd: cwd.as_path(),
        events,
        sink,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cancel,
        lifecycle: None,
        task_registry: None,
        root_task_id: None,
        executor,
    })
}

fn child_loop_setup(runtime_config: &RunConfig) -> ChildAgentLoopSetup {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    prepare_child_agent_loop(
        runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    )
}

#[test]
fn prepare_child_agent_loop_builds_provider_conversation_and_policy() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let setup = prepare_child_agent_loop(
        &config(Some("deepseek-v4-pro")),
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );

    assert_eq!(
        setup.provider_config.model.as_deref(),
        Some(orca_core::model::FLASH_MODEL)
    );
    assert!(setup.provider_config.tools_override.is_some());
    assert!(setup.provider_config.mcp_registry.is_some());
    assert!(setup.context_config.max_tokens > 0);
    assert_eq!(setup.conversation.messages.len(), 2);
    assert!(matches!(
        setup.conversation.messages.first(),
        Some(Message::System { .. })
    ));
    assert!(matches!(
        setup.conversation.messages.get(1),
        Some(Message::User { content, .. }) if content == "inspect repo"
    ));
    assert!(format!("{:?}", setup.policy).contains("Suggest"));
    assert_eq!(setup.turn, 0);
    assert!(!setup.compaction_retry.has_prompt_too_long_retry());
}

#[test]
fn prepare_child_agent_loop_applies_request_tool_allowlist_to_provider_schema() {
    let mut request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    request.allowed_tools = Some(vec!["read_file".to_string()]);
    request.tool_policy_label = Some("review-only".to_string());
    let setup = prepare_child_agent_loop(
        &config(None),
        &request,
        std::env::temp_dir().as_path(),
        &ProjectInstructions::default(),
        &MemoryBlock::default(),
    );

    let tools = setup.provider_config.tools_override.expect("tool override");
    assert_eq!(
        tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>(),
        vec!["read_file"]
    );
}

#[test]
fn advance_child_agent_turn_stops_when_lease_is_exhausted() {
    let runtime_config = config(None);
    let mut setup = child_loop_setup(&runtime_config);
    let mut controller =
        crate::budget_controller::BudgetController::new(orca_core::budget::BudgetSpec {
            max_turns: Some(1),
            ..orca_core::budget::BudgetSpec::default()
        });
    let mut lease = controller
        .child_lease(orca_core::budget::BudgetSpec {
            max_turns: Some(1),
            ..orca_core::budget::BudgetSpec::default()
        })
        .expect("child lease");

    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));
    assert_eq!(setup.turn, 1);

    match advance_child_agent_turn(&mut setup, &mut lease) {
        ChildAgentTurnBudget::Stop(result) => {
            assert_eq!(result.status, RunStatus::Failed);
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("budget stopped"))
            );
        }
        ChildAgentTurnBudget::Continue => panic!("turn beyond lease should stop"),
    }
    assert_eq!(setup.turn, 2);
    assert_eq!(lease.finish().turns, 1);
}

#[test]
fn advance_child_agent_turn_uses_child_config_budget() {
    let runtime_config = config(None);
    let mut setup = child_loop_setup(&runtime_config);
    let mut controller =
        crate::budget_controller::BudgetController::new(runtime_config.budget.to_spec());
    let mut lease = controller
        .child_lease(runtime_config.budget.to_spec())
        .expect("child lease");

    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));

    assert_eq!(setup.turn, 1);
}

#[test]
fn route_child_agent_model_updates_provider_config_and_cost_model() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let mut tracker = CostTracker::new(None);

    let provider_config = route_child_agent_model(&runtime_config, &request, &setup, &mut tracker);
    let totals = tracker.add_usage(Usage {
        input_tokens: 1_000,
        output_tokens: 1_000,
        cache_tokens: 0,
    });

    assert_eq!(
        provider_config.model.as_deref(),
        Some(orca_core::model::PRO_MODEL)
    );
    let expected_pro_cost = (1_000.0 * 0.435 + 1_000.0 * 0.87) / 1_000_000.0;
    assert!((totals.estimated_cost_usd - expected_pro_cost).abs() < 1e-12);
}

#[test]
fn run_child_agent_provider_turn_applies_model_hooks_around_provider_call() {
    let request = ChildAgentRequest::new(
        "mock_system_echo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let provider_config = route_child_agent_model(
        &runtime_config,
        &request,
        &setup,
        &mut CostTracker::new(None),
    );
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "printf runtime-hook-context".to_string(),
        tool: None,
    }]);
    let cancel = CancelToken::new();

    let turn = run_child_agent_provider_turn(
        &runtime_config,
        &setup,
        std::env::temp_dir().as_path(),
        &hooks,
        &provider_config,
        &cancel,
    );

    let ChildAgentProviderTurn::Response(response) = turn else {
        panic!("expected provider response")
    };
    assert!(
        response
            .assistant_content
            .as_deref()
            .unwrap_or_default()
            .contains("runtime-hook-context")
    );
}

#[test]
fn run_child_agent_provider_turn_returns_child_failure_for_model_hook_errors() {
    let request = ChildAgentRequest::new(
        "mock_usage".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let provider_config = route_child_agent_model(
        &runtime_config,
        &request,
        &setup,
        &mut CostTracker::new(None),
    );
    let cancel = CancelToken::new();
    let pre_hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "printf pre-failed >&2; exit 7".to_string(),
        tool: None,
    }]);

    let pre_turn = run_child_agent_provider_turn(
        &runtime_config,
        &setup,
        std::env::temp_dir().as_path(),
        &pre_hooks,
        &provider_config,
        &cancel,
    );

    match pre_turn {
        ChildAgentProviderTurn::Fail { result, usage } => {
            assert!(usage.is_none());
            assert_eq!(result.status, RunStatus::Failed);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("pre_model_call hook failed")
            );
        }
        ChildAgentProviderTurn::Response(_) => panic!("pre hook failure should fail the child"),
    }

    let post_hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PostModelCall,
        command: "printf post-failed >&2; exit 8".to_string(),
        tool: None,
    }]);

    let post_turn = run_child_agent_provider_turn(
        &runtime_config,
        &setup,
        std::env::temp_dir().as_path(),
        &post_hooks,
        &provider_config,
        &cancel,
    );

    match post_turn {
        ChildAgentProviderTurn::Fail { result, usage } => {
            assert_eq!(
                usage,
                Some(Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                })
            );
            assert_eq!(result.status, RunStatus::Failed);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("post_model_call hook failed")
            );
        }
        ChildAgentProviderTurn::Response(_) => {
            panic!("post hook failure should fail the child")
        }
    }
}

#[test]
fn compact_child_agent_conversation_uses_runtime_compaction_step() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mut runtime_config = config(None);
    runtime_config.model_runtime.context_window = Some(128);
    runtime_config.model_runtime.auto_compact_token_limit = Some(64);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    for index in 0..20 {
        setup.conversation.add_user(format!(
            "child message {index}: {}",
            "important context ".repeat(20)
        ));
        setup.conversation.add_assistant(
            Some(format!(
                "child answer {index}: {}",
                "detailed response ".repeat(20)
            )),
            None,
            vec![],
        );
    }
    let before_messages = setup.conversation.messages.len();

    let compacted = compact_child_agent_conversation_if_needed(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
    )
    .expect("child compaction should not fail");

    assert!(compacted);
    assert!(setup.conversation.messages.len() < before_messages);
}

#[test]
fn compact_child_agent_conversation_uses_soft_compaction_limit() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mut runtime_config = config(None);
    runtime_config.model_runtime.context_window = Some(1_000_000);
    runtime_config.model_runtime.auto_compact_token_limit = None;
    runtime_config.model_runtime.soft_compact_token_limit = Some(64);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    for index in 0..20 {
        setup.conversation.add_user(format!(
            "child message {index}: {}",
            "important context ".repeat(20)
        ));
        setup.conversation.add_assistant(
            Some(format!(
                "child answer {index}: {}",
                "detailed response ".repeat(20)
            )),
            None,
            vec![],
        );
    }
    let before_messages = setup.conversation.messages.len();

    let compacted = compact_child_agent_conversation_if_needed(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
    )
    .expect("child compaction should not fail");

    assert!(compacted);
    assert!(setup.conversation.messages.len() < before_messages);
}

#[test]
fn handle_child_agent_provider_error_retries_prompt_too_long_once() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mut runtime_config = config(None);
    runtime_config.model_runtime.context_window = Some(128);
    runtime_config.model_runtime.auto_compact_token_limit = Some(64);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    for index in 0..20 {
        setup.conversation.add_user(format!(
            "child message {index}: {}",
            "important context ".repeat(20)
        ));
        setup.conversation.add_assistant(
            Some(format!(
                "child answer {index}: {}",
                "detailed response ".repeat(20)
            )),
            None,
            vec![],
        );
    }
    let before_messages = setup.conversation.messages.len();
    let response = ProviderResponse {
        steps: vec![ProviderStep::Error("prompt_too_long".to_string())],
        assistant_content: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        usage: None,
    };

    let decision = handle_child_agent_provider_error(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
        &response,
    )
    .expect("provider-error handling should not fail")
    .expect("prompt-too-long should produce a decision");

    assert!(matches!(
        decision,
        ChildAgentProviderErrorDecision::RetryAfterCompaction
    ));
    assert!(setup.compaction_retry.has_prompt_too_long_retry());
    assert!(setup.conversation.messages.len() < before_messages);

    let decision = handle_child_agent_provider_error(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
        &response,
    )
    .expect("provider-error handling should not fail")
    .expect("repeated prompt-too-long should fail");

    match decision {
        ChildAgentProviderErrorDecision::Fail(result) => {
            assert_eq!(result.status, RunStatus::Failed);
            assert_eq!(result.error.as_deref(), Some("prompt_too_long"));
        }
        ChildAgentProviderErrorDecision::RetryAfterCompaction => {
            panic!("repeated prompt-too-long should not retry")
        }
    }
}

#[test]
fn child_agent_provider_error_records_usage_before_failure() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let response = ProviderResponse {
        steps: vec![ProviderStep::Error("quota exhausted".to_string())],
        assistant_content: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        }),
    };
    let mut tracker = CostTracker::new(None);

    let decision = crate::child_agent_loop_runner::handle_child_agent_provider_error_with_usage(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
        &response,
        &mut tracker,
        None,
    )
    .expect("provider-error handling should not fail")
    .expect("provider error should produce a decision");

    assert!(matches!(
        decision,
        ChildAgentProviderErrorDecision::Fail(ChildAgentResult {
            status: RunStatus::Failed,
            ..
        })
    ));
    let totals = tracker.totals();
    assert_eq!(totals.input_tokens, 120);
    assert_eq!(totals.output_tokens, 30);
    assert_eq!(totals.cache_tokens, 10);
}

#[test]
fn observed_child_agent_provider_error_emits_cumulative_usage() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let response = ProviderResponse {
        steps: vec![ProviderStep::Error("quota exhausted".to_string())],
        assistant_content: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        }),
    };
    let mut tracker = CostTracker::new(None);
    tracker.add_usage(Usage {
        input_tokens: 5,
        output_tokens: 2,
        cache_tokens: 1,
    });
    let activities = RefCell::new(Vec::new());
    let observer = ChildAgentActivityObserver::new(|activity| {
        activities.borrow_mut().push(activity.clone());
    });

    let decision = crate::child_agent_loop_runner::handle_child_agent_provider_error_with_usage(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
        &response,
        &mut tracker,
        Some(&observer),
    )
    .expect("provider-error handling should not fail")
    .expect("provider error should produce a decision");

    assert!(matches!(decision, ChildAgentProviderErrorDecision::Fail(_)));
    drop(observer);
    assert_eq!(
        activities.into_inner(),
        vec![ChildAgentActivity::Usage(tracker.totals())]
    );
    let totals = tracker.totals();
    assert_eq!(totals.input_tokens, 125);
    assert_eq!(totals.output_tokens, 32);
    assert_eq!(totals.cache_tokens, 11);
}

#[test]
fn fold_child_agent_provider_response_records_usage_and_terminal_assistant() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let response = ProviderResponse {
        steps: vec![ProviderStep::MessageDelta("done".to_string())],
        assistant_content: Some("done".to_string()),
        assistant_reasoning: Some("reasoned".to_string()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        }),
    };
    let mut tracker = CostTracker::new(Some(orca_core::model::PRO_MODEL));

    let decision = crate::child_agent_loop_runner::handle_child_agent_provider_error_with_usage(
        &runtime_config,
        &mut setup,
        std::env::temp_dir().as_path(),
        &HookRunner::default(),
        &response,
        &mut tracker,
        None,
    )
    .expect("provider-error handling should not fail");
    assert!(decision.is_none());
    let fold = fold_child_agent_provider_response(&mut setup, &response, &mut tracker);

    match fold {
        ChildAgentProviderResponseFold::Complete(result) => {
            assert_eq!(result.status, RunStatus::Success);
            assert_eq!(result.final_message.as_deref(), Some("done"));
        }
        ChildAgentProviderResponseFold::ContinueToTools => {
            panic!("terminal response should complete child run")
        }
    }
    let totals = tracker.totals();
    assert_eq!(totals.input_tokens, 120);
    assert_eq!(totals.output_tokens, 30);
    assert_eq!(totals.cache_tokens, 10);
    assert!(matches!(
        setup.conversation.messages.last(),
        Some(Message::Assistant {
            content: Some(content),
            reasoning_content: Some(reasoning),
            tool_calls,
            ..
        }) if content == "done" && reasoning == "reasoned" && tool_calls.is_empty()
    ));
}

#[test]
fn fold_child_agent_provider_response_records_assistant_before_tools() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let response = ProviderResponse {
        steps: vec![],
        assistant_content: Some("I need a tool".to_string()),
        assistant_reasoning: None,
        tool_calls: vec![RawToolCall {
            id: "tool-1".to_string(),
            function_name: "bash".to_string(),
            arguments: "{\"command\":\"echo hi\"}".to_string(),
        }],
        usage: None,
    };
    let mut tracker = CostTracker::new(None);

    let fold = fold_child_agent_provider_response(&mut setup, &response, &mut tracker);

    assert!(matches!(
        fold,
        ChildAgentProviderResponseFold::ContinueToTools
    ));
    assert!(matches!(
        setup.conversation.messages.last(),
        Some(Message::Assistant {
            content: Some(content),
            tool_calls,
            ..
        }) if content == "I need a tool" && tool_calls.len() == 1
    ));
}

#[test]
fn child_agent_tool_requests_extracts_only_provider_tool_calls() {
    let first = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("echo one".to_string()),
        raw_arguments: None,
    };
    let second = ToolRequest {
        id: "tool-2".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("Cargo.toml".to_string()),
        raw_arguments: None,
    };
    let response = ProviderResponse {
        steps: vec![
            ProviderStep::MessageDelta("before".to_string()),
            ProviderStep::ToolCall(first.clone()),
            ProviderStep::Error("ignored here".to_string()),
            ProviderStep::ToolCall(second.clone()),
        ],
        assistant_content: Some("tool please".to_string()),
        assistant_reasoning: None,
        tool_calls: vec![],
        usage: None,
    };

    let requests = child_agent_tool_requests(&response);

    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].id, first.id);
    assert_eq!(requests[1].id, second.id);
}

#[test]
fn run_child_agent_loop_with_tool_executor_runs_tools_until_provider_completes() {
    let request = ChildAgentRequest::new(
        "bash echo child".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut tracker = CostTracker::new(None);
    let mut tool_count = 0;

    let result = run_child_agent_loop_with_tool_executor(
        &runtime_config,
        ChildAgentLoopContext {
            request: &request,
            cwd: std::env::temp_dir().as_path(),
            instructions: &instructions,
            memory: &memory,
            hooks: &HookRunner::default(),
            child_cost_tracker: &mut tracker,
            lease: None,
        },
        |_setup, _cancel, tool_request| {
            tool_count += 1;
            assert_eq!(tool_request.name, ToolName::Bash);
            assert_eq!(tool_request.target.as_deref(), Some("echo child"));
            ChildAgentToolExecution {
                should_stop: false,
                result: ToolResult::completed(tool_request, "child tool ran".to_string(), false),
                child_cost: None,
            }
        },
    )
    .expect("child loop runner should complete");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(
        result.final_message.as_deref(),
        Some("Mock completed after tool execution.")
    );
    assert_eq!(tool_count, 1);
    let budget_usage = result
        .budget_usage
        .expect("child loop returns an exact budget receipt");
    assert_eq!(budget_usage.turns, 2);
    assert_eq!(budget_usage.tool_calls, 1);
}

#[test]
fn observed_child_agent_stops_at_local_budget_after_emitting_exact_usage() {
    let request = ChildAgentRequest::new(
        "mock_usage".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mut runtime_config = config(None);
    // Any provider spend crosses a 1-micro cost ceiling.
    runtime_config.budget.max_cost_usd_micros = Some(1);
    let mut tracker = CostTracker::new(None);
    let activities = RefCell::new(Vec::new());
    let observer = ChildAgentActivityObserver::new(|activity| {
        activities.borrow_mut().push(activity.clone());
    });

    let result = run_child_agent_loop_with_tool_executor_observed(
        &runtime_config,
        ChildAgentLoopContext {
            request: &request,
            cwd: std::env::temp_dir().as_path(),
            instructions: &instructions,
            memory: &memory,
            hooks: &HookRunner::default(),
            child_cost_tracker: &mut tracker,
            lease: None,
        },
        Some(&observer),
        |_setup, _cancel, _tool_request| {
            panic!("budget-exhausted child must not execute provider-requested tools")
        },
    )
    .expect("child loop should report budget exhaustion");

    assert_eq!(result.status, RunStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("budget stopped"))
    );
    let totals = tracker.totals();
    assert_eq!(totals.input_tokens, 120);
    assert_eq!(totals.output_tokens, 30);
    assert_eq!(totals.cache_tokens, 10);
    let budget_usage = result
        .budget_usage
        .expect("budget-stopped child still returns its consumed receipt");
    assert_eq!(budget_usage.turns, 1);
    assert_eq!(
        budget_usage.cost_usd_micros,
        crate::cost::usd_to_micros(totals.estimated_cost_usd),
        "the lease receipt includes provider spend"
    );
    let usage_events = activities
        .borrow()
        .iter()
        .filter_map(|activity| match activity {
            ChildAgentActivity::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage_events, vec![totals]);
}

#[test]
fn run_child_agent_with_tool_executor_applies_override_and_runs_loop() {
    let request = ChildAgentRequest::new(
        "bash echo child".to_string(),
        SubagentType::General,
        Some(FLASH_MODEL.to_string()),
        3,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut saw_child_config = false;
    let mut tool_count = 0;

    let (result, _tracker) = run_child_agent_with_tool_executor(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
        &HookRunner::default(),
        |child_config, child_request, _tool_context, _cancel, tool_request| {
            saw_child_config = true;
            tool_count += 1;
            assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
            assert_eq!(child_request.depth, 3);
            assert_eq!(tool_request.name, ToolName::Bash);
            ChildAgentToolExecution {
                should_stop: false,
                result: ToolResult::completed(tool_request, "child tool ran".to_string(), false),
                child_cost: None,
            }
        },
    );

    assert_eq!(result.status, RunStatus::Success);
    assert!(saw_child_config);
    assert_eq!(tool_count, 1);
}

#[test]
fn run_child_agent_prompt_with_tool_executor_builds_runtime_request() {
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut saw_request = false;

    let (result, _tracker) = run_child_agent_prompt_with_tool_executor(
        &runtime_config,
        ChildAgentPromptContext {
            prompt: "bash echo child".to_string(),
            subagent_type: &SubagentType::General,
            subagent_model: Some(FLASH_MODEL.to_string()),
            subagent_depth: 4,
            cwd: std::env::temp_dir().as_path(),
            instructions: &instructions,
            memory: &memory,
            hooks: &HookRunner::default(),
        },
        |child_config, child_request, _tool_context, _cancel, tool_request| {
            saw_request = true;
            assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
            assert_eq!(child_request.prompt.as_str(), "bash echo child");
            assert!(matches!(
                &child_request.subagent_type,
                SubagentType::General
            ));
            assert_eq!(child_request.depth, 4);
            assert_eq!(tool_request.name, ToolName::Bash);
            ChildAgentToolExecution {
                should_stop: false,
                result: ToolResult::completed(tool_request, "child tool ran".to_string(), false),
                child_cost: None,
            }
        },
    );

    assert_eq!(result.status, RunStatus::Success);
    assert!(saw_request);
}

#[test]
fn fold_child_agent_tool_result_merges_cost_and_records_model_context() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let tool_request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("echo hi".to_string()),
        raw_arguments: None,
    };
    let result = ToolResult::completed(&tool_request, "hello from tool".to_string(), false);
    let mut nested_cost = CostTracker::new(Some(orca_core::model::PRO_MODEL));
    nested_cost.add_usage(Usage {
        input_tokens: 10,
        output_tokens: 5,
        cache_tokens: 0,
    });
    let mut tracker = CostTracker::new(None);

    let fold = fold_child_agent_tool_result(
        &mut setup,
        &tool_request,
        false,
        result,
        Some(nested_cost),
        &mut tracker,
    );

    assert!(matches!(fold, ChildAgentToolResultFold::Continue));
    assert!(tracker.totals().total_tokens() > 0);
    assert!(matches!(
        setup.conversation.messages.last(),
        Some(Message::Tool {
            tool_call_id,
            content,
            ..
        }) if tool_call_id == "tool-1" && content.contains("hello from tool")
    ));
}

#[test]
fn fold_child_agent_tool_result_turns_stop_into_failed_child_result() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let tool_request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("exit 1".to_string()),
        raw_arguments: None,
    };
    let result = ToolResult::failed(&tool_request, "tool failed", Some(1));
    let mut tracker = CostTracker::new(None);

    let fold =
        fold_child_agent_tool_result(&mut setup, &tool_request, true, result, None, &mut tracker);

    match fold {
        ChildAgentToolResultFold::Stop(result) => {
            assert_eq!(result.status, RunStatus::Failed);
            assert_eq!(result.error.as_deref(), Some("tool failed"));
        }
        ChildAgentToolResultFold::Continue => panic!("should_stop should stop child execution"),
    }
    assert!(matches!(
        setup.conversation.messages.last(),
        Some(Message::Tool {
            tool_call_id,
            content,
            ..
        }) if tool_call_id == "tool-1" && content.contains("tool failed")
    ));
}

#[test]
fn fold_child_agent_tool_result_preserves_cancelled_child_result() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let tool_request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("sleep 30".to_string()),
        raw_arguments: None,
    };
    let result = ToolResult::cancelled(&tool_request, "turn interrupted", Some(130));
    let mut tracker = CostTracker::new(None);

    let fold =
        fold_child_agent_tool_result(&mut setup, &tool_request, true, result, None, &mut tracker);

    match fold {
        ChildAgentToolResultFold::Stop(result) => {
            assert_eq!(result.status, RunStatus::Cancelled);
            assert_eq!(result.error.as_deref(), Some("turn interrupted"));
        }
        ChildAgentToolResultFold::Continue => panic!("should_stop should stop child execution"),
    }
}

#[test]
fn stopping_child_tool_result_closes_three_call_sibling_boundary() {
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let runtime_config = config(None);
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let tool_requests = ["tool-1", "tool-2", "tool-3"]
        .into_iter()
        .map(|id| ToolRequest {
            id: id.to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(format!("echo {id}")),
            raw_arguments: None,
        })
        .collect::<Vec<_>>();
    let raw_calls = tool_requests
        .iter()
        .map(|request| RawToolCall {
            id: request.id.clone(),
            function_name: request.name.as_str().to_string(),
            arguments: request.raw_arguments.clone().unwrap_or_default(),
        })
        .collect();
    setup
        .conversation
        .add_assistant(Some("run tools".to_string()), None, raw_calls);
    let mut tracker = CostTracker::new(None);
    let result = ToolResult::cancelled(&tool_requests[0], "turn interrupted", Some(130));

    let fold = fold_child_agent_tool_result_and_close_siblings(
        &mut setup,
        &tool_requests[0],
        &tool_requests[1..].iter().collect::<Vec<_>>(),
        true,
        result,
        None,
        &mut tracker,
    );

    assert!(matches!(
        fold,
        ChildAgentToolResultFold::Stop(ChildAgentResult {
            status: RunStatus::Cancelled,
            ..
        })
    ));
    let terminal_messages = setup
        .conversation
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } => Some((tool_call_id.as_str(), terminal)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_messages.len(), 3);
    for (index, (tool_call_id, terminal)) in terminal_messages.iter().enumerate() {
        assert_eq!(*tool_call_id, tool_requests[index].id);
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
}

#[test]
fn run_child_agent_applies_subagent_model_override() {
    let request = ChildAgentRequest {
        prompt: "inspect repo".to_string(),
        subagent_type: SubagentType::General,
        model: Some(FLASH_MODEL.to_string()),
        depth: 1,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let cancel = CancelToken::new();
    let mut events = EventFactory::new("test-run".to_string());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
        assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("ok".to_string()),
            error: None,
            budget_usage: None,
        })
    });

    let (result, _) = run_child_agent(&config(None), &request, &mut runtime);

    assert_eq!(result.status, RunStatus::Success);
}

#[test]
fn run_child_agent_ignores_auto_override() {
    let request = ChildAgentRequest {
        prompt: "inspect repo".to_string(),
        subagent_type: SubagentType::General,
        model: Some(AUTO_MODEL.to_string()),
        depth: 1,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let cancel = CancelToken::new();
    let mut events = EventFactory::new("test-run".to_string());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
        assert_eq!(child_config.model.as_deref(), Some("deepseek-v4-pro"));
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: None,
            error: None,
            budget_usage: None,
        })
    });

    let _ = run_child_agent(&config(Some("deepseek-v4-pro")), &request, &mut runtime);
}

#[test]
fn run_child_agent_preserves_cost_tracker_on_loop_error() {
    let request = ChildAgentRequest {
        prompt: "inspect repo".to_string(),
        subagent_type: SubagentType::General,
        model: None,
        depth: 1,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let cancel = CancelToken::new();
    let mut events = EventFactory::new("test-run".to_string());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let mut runtime = runtime(&mut sink, &mut events, &cancel, |_, _, _, tracker| {
        tracker.add_usage(Usage {
            input_tokens: 7,
            output_tokens: 3,
            cache_tokens: 2,
        });
        Err(io::Error::other("child loop failed"))
    });

    let (result, tracker) = run_child_agent(&config(None), &request, &mut runtime);

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("child loop failed"));
    let tracker_debug = format!("{tracker:?}");
    assert!(tracker_debug.contains("input_tokens: 7"));
    assert!(tracker_debug.contains("output_tokens: 3"));
}
