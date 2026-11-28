use std::cell::RefCell;
use std::io;
use std::io::Cursor;
use std::sync::Arc;

use crate::agent_child::*;
use crate::cost::CostTracker;
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::{ActionKind, ApprovalMode};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::conversation::{ImageDetail, ImageInput, ImageSource, Message, RawToolCall};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::external_config::ExternalToolConfig;
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::mcp_types::McpServerConfig;
use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection, PRO_MODEL};
use orca_core::provider_types::{
    ProviderError, ProviderErrorKind, ProviderResponse, ProviderStep, Usage,
};
use orca_core::subagent_config::SubagentConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::thread_identity::TurnId;
use orca_core::tool_images::TOOL_IMAGE_UNAVAILABLE_NOTE;
use orca_core::tool_types::{ToolInvocationStarted, ToolName, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use crate::child_agent_provider_turn::child_agent_model_conversation;
use crate::child_agent_response_folding::fold_child_agent_tool_result_and_close_siblings;
use crate::hooks::{HookOutcome, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

fn config(model: Option<&str>) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::Suggest,
        startup_approval_mode: None,
        execution_profile: orca_core::capability::ExecutionProfile::Workspace,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(model.map(str::to_string)),
        model_runtime: Default::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::<McpServerConfig>::new(),
        mcp_credentials_path: None,
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
        computer_use: orca_core::config::ComputerUseConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        pricing_refresh: false,
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
        child_task_id: None,
        checkpoint_observer: None,
        permission_handler: None,
        turn_id: None,
        executor,
    })
}

fn noop_child_executor<W: io::Write>(
    _: &RunConfig,
    _: &ChildAgentRequest,
    _: &mut ChildAgentRuntime<'_, W>,
    _: &mut CostTracker,
) -> io::Result<ChildAgentResult> {
    Ok(ChildAgentResult {
        status: RunStatus::Success,
        final_message: None,
        error: None,
        budget_usage: None,
    })
}

#[test]
fn child_runtime_retains_owned_permission_handler() {
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mcp_registry = McpRegistry::default();
    let hooks = HookRunner::new(Vec::new());
    let cancel = CancelToken::new();
    let mut events = EventFactory::new("permission-handler-retained".to_string());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let handler: Arc<dyn crate::runtime_permission::RuntimePermissionRequestHandler + Send + Sync> =
        Arc::new(crate::runtime_permission::AllowRequestedPermissions);
    let runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
        cwd: std::path::Path::new("."),
        events: &mut events,
        sink: &mut sink,
        instructions: &instructions,
        memory: &memory,
        mcp_registry: &mcp_registry,
        hooks: &hooks,
        cancel: &cancel,
        lifecycle: None,
        task_registry: None,
        root_task_id: None,
        child_task_id: None,
        checkpoint_observer: None,
        permission_handler: Some(handler),
        turn_id: Some(TurnId::new()),
        executor: noop_child_executor::<Cursor<Vec<u8>>>,
    });

    assert!(runtime.permission_handler.is_some());
    assert!(runtime.turn_id.is_some());
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
    // Seeded with a different model than the auto route resolves to, so the
    // assertion below actually proves route_child_agent_model overrides the
    // tracker's pricing rather than merely leaving it unchanged.
    let mut tracker = CostTracker::new(Some(orca_core::model::PRO_MODEL))
        .with_clock(crate::pricing::rates::off_peak_instant);

    let provider_config =
        route_child_agent_model(&runtime_config, &request, &setup, &mut tracker).provider_config;
    let totals = tracker.add_usage(Usage {
        input_tokens: 1_000,
        output_tokens: 1_000,
        cache_tokens: 0,
    });

    assert_eq!(provider_config.model.as_deref(), Some(FLASH_MODEL));
    let expected_flash_cost = (1_000.0 * 0.15 + 1_000.0 * 0.6) / 1_000_000.0;
    assert!((totals.estimated_cost_usd - expected_flash_cost).abs() < 1e-12);
}

#[test]
fn a_child_agent_on_a_model_that_cannot_see_images_gets_a_note() {
    // Routes a child whose conversation holds one tool image and returns the
    // tool message the child sends to that model.
    fn sent_tool_message(model: &str) -> Message {
        let runtime_config = config(Some(model));
        let mut setup = child_loop_setup(&runtime_config);
        setup.conversation.messages.push(Message::Assistant {
            cut_short: None,
            content: None,
            reasoning_content: None,
            tool_calls: vec![RawToolCall {
                id: "call-screenshot".to_string(),
                function_name: "mcp__screen__capture".to_string(),
                arguments: "{}".to_string(),
            }],
            pinned: false,
        });
        setup.conversation.messages.push(Message::Tool {
            tool_call_id: "call-screenshot".to_string(),
            content: "screenshot taken\n[1 image attached]".to_string(),
            images: vec![ImageInput {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string(),
                },
                detail: ImageDetail::High,
            }],
            terminal: None,
            pinned: false,
        });
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let model_turn = route_child_agent_model(
            &runtime_config,
            &request,
            &setup,
            &mut CostTracker::new(None),
        );

        let sent = child_agent_model_conversation(
            &setup.conversation,
            &HookOutcome::default(),
            &model_turn,
        );

        assert!(
            matches!(
                setup.conversation.messages.last(),
                Some(Message::Tool { images, .. }) if images.len() == 1
            ),
            "the child's own conversation keeps its image"
        );
        sent.messages
            .into_iter()
            .rev()
            .find(|message| matches!(message, Message::Tool { .. }))
            .expect("the child sends its tool message")
    }

    let Message::Tool {
        content, images, ..
    } = sent_tool_message(PRO_MODEL)
    else {
        panic!("expected a tool message");
    };
    assert!(images.is_empty());
    assert!(content.ends_with(TOOL_IMAGE_UNAVAILABLE_NOTE));

    for model in [FLASH_MODEL, AUTO_MODEL] {
        let Message::Tool {
            content, images, ..
        } = sent_tool_message(model)
        else {
            panic!("expected a tool message");
        };
        assert_eq!(images.len(), 1, "{model} keeps its tool image");
        assert!(!content.contains(TOOL_IMAGE_UNAVAILABLE_NOTE));
    }
}

#[test]
fn a_child_agent_on_a_custom_vision_model_gets_a_note_instead_of_tool_images() {
    const CUSTOM_MODEL: &str = "provider/multimodal-model";
    let mut runtime_config = config(Some(CUSTOM_MODEL));
    runtime_config.model = ModelSelection::parse_with_models(
        Some(CUSTOM_MODEL.to_string()),
        std::collections::BTreeMap::from([(
            CUSTOM_MODEL.to_string(),
            orca_core::model::ModelDefinition {
                supports_images: Some(true),
                ..orca_core::model::ModelDefinition::default()
            },
        )]),
    )
    .expect("custom model selection");
    let image = ImageInput {
        source: ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string(),
        },
        detail: ImageDetail::High,
    };
    let mut setup = child_loop_setup(&runtime_config);
    setup.conversation.messages.push(Message::user_with_images(
        "compare with this".to_string(),
        vec![image.clone()],
    ));
    setup.conversation.messages.push(Message::Assistant {
        cut_short: None,
        content: None,
        reasoning_content: None,
        tool_calls: vec![RawToolCall {
            id: "call-screenshot".to_string(),
            function_name: "mcp__screen__capture".to_string(),
            arguments: "{}".to_string(),
        }],
        pinned: false,
    });
    setup.conversation.messages.push(Message::Tool {
        tool_call_id: "call-screenshot".to_string(),
        content: "screenshot taken\n[1 image attached]".to_string(),
        images: vec![image],
        terminal: None,
        pinned: false,
    });
    let request = ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        2,
        false,
    );
    let model_turn = route_child_agent_model(
        &runtime_config,
        &request,
        &setup,
        &mut CostTracker::new(None),
    );
    assert_eq!(
        model_turn.decision.image_route,
        orca_core::model::ImageRouteDecision::Direct
    );

    let sent =
        child_agent_model_conversation(&setup.conversation, &HookOutcome::default(), &model_turn);

    assert!(
        sent.messages
            .iter()
            .any(|message| matches!(message, Message::User { images, .. } if images.len() == 1)),
        "the user image still reaches the model"
    );
    let Some(Message::Tool {
        content, images, ..
    }) = sent.messages.last()
    else {
        panic!("the tool message must stay last");
    };
    assert!(images.is_empty());
    assert!(content.ends_with(TOOL_IMAGE_UNAVAILABLE_NOTE));
    assert!(
        matches!(
            setup.conversation.messages.last(),
            Some(Message::Tool { images, .. }) if images.len() == 1
        ),
        "the child's own conversation keeps its image"
    );
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
    let model_turn = route_child_agent_model(
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
        &model_turn,
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
    let model_turn = route_child_agent_model(
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
        &model_turn,
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
        &model_turn,
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
        steps: vec![ProviderStep::Error(ProviderError::new(
            ProviderErrorKind::ContextExceeded,
            "prompt_too_long",
        ))],
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
fn child_that_cannot_fit_a_reply_fails_without_sending_the_user_to_new() {
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
    // No window this small leaves room for a minimal reply, and a child's
    // opening request holds nothing compaction could remove.
    runtime_config.model_runtime.context_window = Some(1_000);
    let mut tracker = CostTracker::new(None);

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
        |_setup, _cancel, _tool_request| unreachable!("the child sends no request"),
    )
    .expect("child loop runner should return a result");

    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.expect("the child reports why it stopped");
    assert!(error.contains("sub-agent"), "{error}");
    assert!(!error.contains("/new"), "{error}");
}

#[test]
fn child_prompt_too_long_that_cannot_shrink_fails_without_sending_the_user_to_new() {
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
    // Only the system prompt and the task: nothing compaction could remove.
    let mut setup = prepare_child_agent_loop(
        &runtime_config,
        &request,
        std::env::temp_dir().as_path(),
        &instructions,
        &memory,
    );
    let response = ProviderResponse {
        steps: vec![ProviderStep::Error(ProviderError::new(
            ProviderErrorKind::ContextExceeded,
            "prompt_too_long",
        ))],
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

    let ChildAgentProviderErrorDecision::Fail(result) = decision else {
        panic!("an unshrinkable child must not retry");
    };
    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.expect("the child reports why it stopped");
    assert!(error.starts_with("prompt_too_long"), "{error}");
    assert!(error.contains("sub-agent"), "{error}");
    assert!(!error.contains("/new"), "{error}");
}

#[test]
fn a_child_reply_at_the_output_limit_is_kept() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = config(None);
    config.provider = ProviderKind::DeepSeek;
    config.api_key = Some("test-only".into());
    config.base_url = Some(format!("http://{}", listener.local_addr().unwrap()));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = BufReader::new(&mut stream);
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(request.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        request.read_exact(&mut vec![0; content_length]).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.write_all(br#"data: {"choices":[{"delta":{"content":"Child text","reasoning_content":"Child reasoning","tool_calls":[{"index":0,"id":"call_1","function":{"name":"task_list","arguments":"{"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"length"}]}

data: [DONE]

"#).unwrap();
    });
    let cwd = tempfile::tempdir().unwrap();
    let request =
        ChildAgentRequest::new("inspect repo".into(), SubagentType::General, None, 2, false);
    let mut checkpoint = Vec::new();
    let observer = crate::child_agent_types::ChildAgentCheckpointObserver::new_with_tool_boundary(
        |observation| {
            checkpoint = observation.conversation.messages.clone();
            Ok(())
        },
        |_| panic!("an unfinished tool must not start"),
    );
    let result =
        crate::child_agent_loop_runner::run_child_agent_loop_with_tool_executor_checkpointed(
            &config,
            ChildAgentLoopContext {
                request: &request,
                cwd: cwd.path(),
                instructions: &ProjectInstructions::default(),
                memory: &MemoryBlock::default(),
                hooks: &HookRunner::default(),
                child_cost_tracker: &mut CostTracker::new(None),
                lease: None,
            },
            Some(&observer),
            |_, _, _| panic!("an unfinished tool must not execute"),
        )
        .unwrap();
    drop(observer);
    server.join().unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert!(result.error.unwrap().contains("finish_reason=length"));
    assert!(matches!(checkpoint.last(), Some(Message::Assistant {
        content: Some(content), reasoning_content: Some(reasoning), tool_calls,
        cut_short: Some(orca_core::conversation::CutShort::OutputLimit), ..
    }) if content == "Child text" && reasoning == "Child reasoning" && tool_calls.is_empty()));
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
        steps: vec![ProviderStep::Error(ProviderError::other("quota exhausted"))],
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
            ProviderStep::Error(ProviderError::other("ignored here")),
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
fn child_compaction_allows_the_last_admitted_turn_in_both_loops() {
    use crate::budget_controller::BudgetController;
    use orca_core::budget::BudgetSpec;

    for observed in [false, true] {
        let config = config(None);
        let request =
            ChildAgentRequest::new("mock_usage".into(), SubagentType::General, None, 2, false);
        let mut lease = BudgetController::new(BudgetSpec::default())
            .child_lease(BudgetSpec {
                max_turns: Some(1),
                ..Default::default()
            })
            .unwrap();
        let mut tracker = CostTracker::new(None);
        let cwd = std::env::temp_dir();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let context = ChildAgentLoopContext {
            request: &request,
            cwd: &cwd,
            instructions: &instructions,
            memory: &memory,
            hooks: &hooks,
            child_cost_tracker: &mut tracker,
            lease: Some(&mut lease),
        };
        let result = if observed {
            run_child_agent_loop_with_tool_executor_observed(
                &config,
                context,
                None,
                |_, _, _| unreachable!(),
            )
        } else {
            run_child_agent_loop_with_tool_executor(&config, context, |_, _, _| unreachable!())
        }
        .unwrap();
        assert_eq!(result.status, RunStatus::Success, "{:?}", result.error);
        assert_eq!(tracker.totals().input_tokens, 120);
        assert!(lease.admit_turn().is_err());
    }
}

fn run_child_compaction_fixture(
    retry: bool,
    expire: bool,
) -> (io::Result<ChildAgentResult>, usize, u64, u64) {
    use crate::budget_controller::BudgetController;
    use orca_core::budget::BudgetSpec;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let done_for_server = Arc::clone(&done);
    let server = std::thread::spawn(move || {
        let mut calls = 0;
        'accept: while !done_for_server.load(Ordering::Relaxed) {
            let (mut socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("accept child request: {error}"),
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = 0;
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => continue 'accept,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue 'accept;
                    }
                    Err(error) => panic!("read child request: {error}"),
                }
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            calls += 1;
            let request: serde_json::Value = serde_json::from_slice(&request_body).unwrap();
            let last_message = request["messages"].as_array().unwrap().last().unwrap();
            let content = last_message["content"].as_str().unwrap();
            let replay_call = if retry { 4 } else { 3 };
            if calls == replay_call {
                assert!(content.starts_with("Write a checkpoint"));
            } else if calls > replay_call {
                assert!(content.starts_with("Summarize this collapsed conversation segment"));
            }
            let (status, content_type, body) = if calls <= 2 {
                let chunk = serde_json::json!({"choices":[{"delta":{
                    "content": if calls == 1 { "older evidence ".repeat(2_000) } else { "recent evidence ".repeat(900) },
                    "reasoning_content":"check evidence",
                    "tool_calls":[{"index":0,"id":format!("call-{calls}"),"function":{"name":"bash","arguments":"{\"command\":\"echo evidence\"}"}}]
                },"finish_reason":"tool_calls"}]});
                (
                    "200 OK",
                    "text/event-stream",
                    format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                )
            } else if retry && calls == 3 {
                (
                    "400 Bad Request",
                    "application/json",
                    r#"{"error":{"message":"prompt_too_long"}}"#.to_string(),
                )
            } else {
                if expire && calls == if retry { 4 } else { 3 } {
                    std::thread::sleep(Duration::from_millis(1_400));
                }
                ("200 OK", "text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"short facts\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":20}}\n\ndata: [DONE]\n\n".to_string())
            };
            let _ = write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
        calls
    });
    let mut config = config(Some(PRO_MODEL));
    config.provider = ProviderKind::DeepSeek;
    config.api_key = Some("local-fixture".into());
    config.base_url = Some(base_url);
    if !retry {
        config.model_runtime.soft_compact_token_limit = Some(1);
    }
    let request =
        ChildAgentRequest::new("inspect repo".into(), SubagentType::General, None, 4, false);
    let mut lease = BudgetController::new(BudgetSpec::default())
        .child_lease(BudgetSpec {
            max_wall_time_ms: expire.then_some(1_000),
            ..Default::default()
        })
        .unwrap();
    let mut tracker = CostTracker::new(None);
    let observer = ChildAgentActivityObserver::new_fallible(|activity| {
        if !expire && matches!(activity, ChildAgentActivity::Usage(_)) {
            return Err(io::Error::other("usage observer failed"));
        }
        Ok(())
    });
    let result = run_child_agent_loop_with_tool_executor_observed(
        &config,
        ChildAgentLoopContext {
            request: &request,
            cwd: std::env::temp_dir().as_path(),
            instructions: &ProjectInstructions::default(),
            memory: &MemoryBlock::default(),
            hooks: &HookRunner::default(),
            child_cost_tracker: &mut tracker,
            lease: Some(&mut lease),
        },
        Some(&observer),
        |_, _, request| ChildAgentToolExecution {
            should_stop: false,
            result: ToolResult::completed(request, "done".into(), false),
            child_cost: None,
        },
    );
    done.store(true, Ordering::Relaxed);
    (
        result,
        server.join().unwrap(),
        lease.usage().cost_usd_micros,
        crate::cost::usd_to_micros(tracker.totals().estimated_cost_usd),
    )
}

#[test]
fn child_compaction_settles_lease_before_publishing_usage() {
    for retry in [false, true] {
        let (result, calls, receipt, spent) = run_child_compaction_fixture(retry, false);
        assert_eq!(
            calls,
            if retry { 5 } else { 4 },
            "Flash fallback before expiry"
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "usage observer failed",
            "retry={retry}"
        );
        assert!(spent > 0, "retry={retry}");
        assert_eq!(receipt, spent, "retry={retry}");
    }
}

#[test]
fn child_compaction_stops_remote_requests_at_lease_deadline() {
    for retry in [true, false] {
        let (result, calls, _, spent) = run_child_compaction_fixture(retry, true);
        let result = result.unwrap();
        assert_eq!(
            calls,
            if retry { 4 } else { 3 },
            "no Flash fallback after replay expires, retry={retry}"
        );
        assert_eq!(
            spent, 0,
            "the delayed response arrived after the lease expired"
        );
        assert!(result.error.unwrap().contains("wall_time_budget"));
    }
}

/// What a `bash echo child` child costs when its one tool call was billed
/// `fee`.
fn child_cost_with_tool_fee(fee: orca_core::tool_types::ToolServiceUsage) -> f64 {
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
    run_child_agent_loop_with_tool_executor(
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
        |_setup, _cancel, tool_request| ChildAgentToolExecution {
            should_stop: false,
            result: ToolResult::completed(tool_request, "child tool ran".to_string(), false)
                .with_service_usage(fee),
            child_cost: None,
        },
    )
    .expect("child loop runner should complete");
    tracker.totals().estimated_cost_usd
}

#[test]
fn a_child_tool_fee_reaches_the_child_cost() {
    let unbilled = child_cost_with_tool_fee(orca_core::tool_types::ToolServiceUsage::default());
    let billed = child_cost_with_tool_fee(orca_core::tool_types::ToolServiceUsage {
        deepseek_searches: 1,
        ..orca_core::tool_types::ToolServiceUsage::default()
    });

    // One DeepSeek search: $0.75 per 1,000.
    assert!(
        (billed - unbilled - 0.00075).abs() < 1e-12,
        "{billed} vs {unbilled}"
    );
}

#[test]
fn an_observed_child_publishes_its_tool_fee_at_once() {
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
    let activities = RefCell::new(Vec::new());
    let observer = ChildAgentActivityObserver::new(|activity| {
        activities.borrow_mut().push(activity.clone());
    });

    run_child_agent_loop_with_tool_executor_observed(
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
        |_setup, _cancel, tool_request| ChildAgentToolExecution {
            should_stop: false,
            result: ToolResult::completed(tool_request, "searched".to_string(), false)
                .with_service_usage(orca_core::tool_types::ToolServiceUsage {
                    deepseek_searches: 1,
                    ..orca_core::tool_types::ToolServiceUsage::default()
                }),
            child_cost: None,
        },
    )
    .expect("child loop runner should complete");

    drop(observer);
    let activities = activities.into_inner();
    let completed = activities
        .iter()
        .position(|activity| matches!(activity, ChildAgentActivity::ToolCompleted { .. }))
        .expect("the tool completed");
    assert!(
        matches!(
            activities.get(completed + 1),
            Some(ChildAgentActivity::Usage(totals)) if totals.estimated_cost_usd >= 0.00075 - 1e-12
        ),
        "the fee is not reported right after the call: {activities:?}"
    );
}

#[test]
fn read_only_child_converges_to_a_tool_free_summary_turn() {
    let request = ChildAgentRequest::new(
        "mock_repeat_read 100".to_string(),
        SubagentType::Explorer,
        None,
        2,
        false,
    );
    let instructions = ProjectInstructions::default();
    let memory = MemoryBlock::default();
    let mut runtime_config = config(None);
    runtime_config.subagents.max_investigation_turns = 20;
    runtime_config.subagents.max_investigation_tool_calls = 2;
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
            assert_eq!(tool_request.name, ToolName::ReadFile);
            ChildAgentToolExecution {
                should_stop: false,
                result: ToolResult::completed(tool_request, "evidence".to_string(), false),
                child_cost: None,
            }
        },
    )
    .expect("read-only child should soft-land with a final report");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(tool_count, 2);
    let usage = result.budget_usage.expect("exact child usage");
    assert_eq!(usage.tool_calls, 2);
    assert_eq!(usage.turns, 3);
    assert!(result.final_message.is_some());
}

#[test]
fn hosted_child_kernel_enforces_read_only_convergence() {
    let request = ChildAgentRequest::new(
        "mock_repeat_read 100".to_string(),
        SubagentType::Explorer,
        None,
        1,
        false,
    );
    let mut runtime_config = config(None);
    runtime_config.subagents.max_investigation_turns = 20;
    runtime_config.subagents.max_investigation_tool_calls = 2;
    let cancel = CancelToken::new();
    let mut events = EventFactory::new("hosted-convergence".to_string());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let mut runtime = runtime(
        &mut sink,
        &mut events,
        &cancel,
        crate::agent_loop::execute_child_agent_loop::<Cursor<Vec<u8>>>,
    );

    let (result, _) = run_child_agent(&runtime_config, &request, &mut runtime);

    assert_eq!(result.status, RunStatus::Success, "{:?}", result.error);
    assert!(result.final_message.is_some());
    let usage = result.budget_usage.expect("hosted child usage");
    assert!(usage.tool_calls <= 2, "{usage:?}");
    assert_eq!(usage.turns, 3);
}

#[test]
fn activity_sink_failure_before_tool_started_prevents_tool_execution() {
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
    let tool_executed = RefCell::new(false);
    let observer = ChildAgentActivityObserver::new_fallible(|activity| {
        if matches!(activity, ChildAgentActivity::ToolStarted { .. }) {
            return Err(io::Error::other("injected activity admission failure"));
        }
        Ok(())
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
            *tool_executed.borrow_mut() = true;
            panic!("tool execution must be fenced by ToolStarted activity admission")
        },
    );

    assert!(result.is_err());
    assert!(!*tool_executed.borrow());
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
        headless_root: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
        continuation: None,
        fork_prefix: None,
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
        headless_root: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
        continuation: None,
        fork_prefix: None,
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
        headless_root: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
        continuation: None,
        fork_prefix: None,
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

fn fork_tool_request(context: Option<&str>) -> ToolRequest {
    let mut arguments = serde_json::json!({
        "description": "inspect parent evidence",
        "prompt": "Report the relevant evidence.",
        "subagent_type": "explorer",
        "model": "deepseek-flash"
    });
    if let Some(context) = context {
        arguments["context"] = context.into();
    }
    ToolRequest {
        id: "fork-child".into(),
        name: ToolName::Subagent,
        action: ActionKind::Agent,
        target: None,
        raw_arguments: Some(arguments.to_string()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordedHistory {
    Empty,
    Ordinary,
    OpeningCompaction,
    ImageRewrite,
    Interrupted,
}

fn recorded_provider(
    replies: Vec<serde_json::Value>,
    mut observe: impl FnMut(usize, &[u8], &mut serde_json::Value) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for (index, mut delta) in replies.into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return bodies;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("provider accept failed: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = 0;
            loop {
                let mut header = String::new();
                assert_ne!(reader.read_line(&mut header).unwrap(), 0);
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            observe(index, &body, &mut delta);
            bodies.push(body);
            let finish = if delta.get("tool_calls").is_some() {
                "tool_calls"
            } else {
                "stop"
            };
            let chunk = serde_json::json!({"choices":[{"delta":delta,"finish_reason":finish}]});
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        bodies
    });
    (base_url, server)
}

fn run_recorded_fork(
    context: Option<&str>,
    history: RecordedHistory,
    attempt_write: bool,
) -> (Vec<Vec<u8>>, ChildAgentResult) {
    let mut replies = Vec::new();
    if attempt_write {
        replies.push(serde_json::json!({"reasoning_content":"try a write", "tool_calls":[{
            "index":0,"id":"child-write","function":{"name":"write_file", "arguments":"{\"path\":\"fork-write.txt\",\"content\":\"unexpected\"}"}
        }]}));
    }
    run_recorded_fork_with(context, history, replies, |_, _| {})
}

fn run_recorded_fork_with(
    context: Option<&str>,
    history: RecordedHistory,
    mut child_replies: Vec<serde_json::Value>,
    configure: impl FnOnce(&std::path::Path, &mut RunConfig),
) -> (Vec<Vec<u8>>, ChildAgentResult) {
    use orca_core::conversation::Conversation;
    use orca_provider::tool_schema::ProviderToolDefinition;

    let cwd = tempfile::tempdir().unwrap();
    let orca_home = tempfile::tempdir().unwrap();
    let _home = crate::history::redirect_test_orca_home(orca_home.path());
    let mut replies = vec![serde_json::json!({"content": "parent response"})];
    replies.append(&mut child_replies);
    replies.push(serde_json::json!({"content": "child report"}));
    let (base_url, server) = recorded_provider(replies, |_, _, _| {});
    let mut config = config(Some(PRO_MODEL));
    config.provider = ProviderKind::DeepSeek;
    config.approval_mode = ApprovalMode::FullAuto;
    config.api_key = Some("local-fixture".into());
    config.base_url = Some(base_url);
    configure(cwd.path(), &mut config);
    let completed = history != RecordedHistory::Empty;
    let mut parent = Conversation::new();
    parent.add_system("Recorded parent system prompt".into());
    if completed {
        parent.add_user("Describe the recorded evidence".into());
        parent.add_assistant(
            None,
            Some("recorded reasoning".into()),
            vec![RawToolCall {
                id: "parent-read".into(),
                function_name: "read_file".into(),
                arguments: "{}".into(),
            }],
        );
        parent.add_tool_result(
            "parent-read".into(),
            "recorded evidence from the completed turn\n".repeat(512),
        );
        parent.add_assistant(Some("parent completed report".into()), None, vec![]);
    }
    if history == RecordedHistory::Interrupted {
        // The prior turn was cancelled before it produced any reply.
        parent.messages.truncate(2);
    }
    if history == RecordedHistory::ImageRewrite {
        let Message::Tool { images, .. } = &mut parent.messages[3] else {
            panic!("expected earlier tool result");
        };
        images.push(ImageInput {
            source: ImageSource::Url {
                url: "https://example.invalid/evidence.png".into(),
            },
            detail: ImageDetail::Low,
        });
    }
    if history == RecordedHistory::OpeningCompaction {
        parent.add_user("prior compaction evidence ".repeat(1500));
        parent.add_assistant(Some("prior report".into()), None, vec![]);
    }
    parent.add_user("in-flight parent prompt".into());
    parent.start_provider_turn("parent-turn");
    if history == RecordedHistory::OpeningCompaction {
        parent = orca_provider::context::compact(
            &parent,
            &orca_provider::context::ContextConfig {
                max_tokens: 400,
                reserved_for_response: 100,
                ..Default::default()
            },
        );
        assert!(
            parent.summary.baseline.is_some(),
            "compaction should leave a summary"
        );
    }

    if completed {
        parent.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "active-tool".into(),
                function_name: "read_file".into(),
                arguments: "{}".into(),
            }],
        );
        parent.add_tool_result("active-tool".into(), "in-flight result".into());
        parent.add_user("in-flight steering".into());
        parent.note_provider_turn_steer();
    }
    let registry = orca_tools::registry::default_tool_registry();
    let parent_config = orca_provider::ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(PRO_MODEL.into()),
        reasoning_effort: orca_core::config::ReasoningEffort::High,
        tools_override: Some(
            ["write_file", "read_file"]
                .map(|name| {
                    let spec = registry.get(name).unwrap().spec();
                    ProviderToolDefinition {
                        name: name.into(),
                        description: spec.description.clone(),
                        input_schema: spec.input_schema.clone(),
                        strict_capable: false,
                    }
                })
                .to_vec(),
        ),
        mcp_registry: None,
        external_tools: vec![],
        max_output_tokens: None,
    };
    let cancel = CancelToken::new();
    let mut prepared = parent.clone();
    orca_core::tool_images::drop_tool_images(&mut prepared.messages, TOOL_IMAGE_UNAVAILABLE_NOTE);
    orca_provider::deepseek_http::bind_replay_source(&mut prepared, &parent);
    let recorded = orca_provider::call_streaming(
        ProviderKind::DeepSeek,
        &prepared,
        &parent_config,
        &cancel,
        &mut |_| {},
    );
    assert_eq!(
        recorded.assistant_content.as_deref(),
        Some("parent response")
    );
    if completed {
        parent.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "pending-tool".into(),
                function_name: "subagent".into(),
                arguments: "{}".into(),
            }],
        );
    }
    let mut request = crate::subagent::create_subagent_request(&fork_tool_request(context));
    let registry = crate::tasks::TaskRegistry::new("recorded-fork".into());
    crate::subagent::prepare_fork_request(&mut request, &config, Some(&parent), &registry).unwrap();
    let mut child = ChildAgentRequest::new(
        request.prompt,
        request.subagent_type,
        request.model,
        1,
        false,
    );
    child.fork_prefix = request.fork_prefix_ref.map(|reference| {
        registry
            .continuation_store()
            .unwrap()
            .load_fork_prefix(reference)
            .unwrap()
    });
    let mut events = EventFactory::new("fork-child".into());
    let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
    let mut runtime = runtime(
        &mut sink,
        &mut events,
        &cancel,
        crate::agent_loop::execute_child_agent_loop,
    );
    runtime.cwd = cwd.path();
    let (result, _) = run_child_agent(&config, &child, &mut runtime);
    assert!(!cwd.path().join("fork-write.txt").exists());
    (server.join().unwrap(), result)
}

fn recorded_wire_messages(body: &[u8]) -> Vec<Box<serde_json::value::RawValue>> {
    #[derive(serde::Deserialize)]
    struct Request {
        messages: Vec<Box<serde_json::value::RawValue>>,
    }
    serde_json::from_slice::<Request>(body).unwrap().messages
}

#[test]
fn a_fork_childs_first_request_shares_the_parents_prefix() {
    let (bodies, result) = run_recorded_fork(Some("fork"), RecordedHistory::Ordinary, false);
    let parent = recorded_wire_messages(&bodies[0]);
    let child = recorded_wire_messages(&bodies[1]);
    let inherited = 5;
    for (parent, child) in parent[..inherited].iter().zip(&child[..inherited]) {
        assert_eq!(parent.get(), child.get());
    }
    assert!(
        child[2]
            .get()
            .contains(r#""reasoning_content":"recorded reasoning""#)
    );
    assert!(
        child[inherited]
            .get()
            .contains("Report the relevant evidence")
    );
    assert!(child[inherited].get().contains("read-only"));
    #[derive(serde::Deserialize)]
    struct Tools {
        tools: Box<serde_json::value::RawValue>,
    }
    let tools = |body: &[u8]| serde_json::from_slice::<Tools>(body).unwrap().tools;
    assert_eq!(tools(&bodies[0]).get(), tools(&bodies[1]).get());
    let matching_message_bytes: usize = child[..inherited]
        .iter()
        .map(|message| message.get().len())
        .sum();
    let shared = matching_message_bytes + tools(&bodies[1]).get().len();
    eprintln!(
        "Recorded fork share: {shared}/{} request bytes ({:.2}%)",
        bodies[1].len(),
        100.0 * shared as f64 / bodies[1].len() as f64
    );
    assert_eq!(result.final_message.as_deref(), Some("child report"));
}

#[test]
fn a_hosted_fork_childs_first_request_has_the_inherited_messages() {
    let (parent, child) = recorded_hosted_fork(false, false, None);
    assert_hosted_fork_inherits(&parent, &child);
}

#[test]
fn a_hosted_fork_that_outlives_its_lease_settles_normally() {
    let (parent, child) = recorded_hosted_fork(false, false, Some(1_000));
    assert_hosted_fork_inherits(&parent, &child);
}

#[test]
fn a_fork_recovered_from_the_queue_still_inherits() {
    let (parent, child) = recorded_hosted_fork(true, false, None);
    assert_hosted_fork_inherits(&parent, &child);
}

#[test]
fn a_hosted_fork_without_a_usable_prefix_says_why_it_started_fresh() {
    let (_, child) = recorded_hosted_fork(false, true, None);
    let child = std::str::from_utf8(&child).unwrap();
    assert!(child.contains("Fork context unavailable:"), "{child}");
    assert!(
        child.contains("referenced fork prefix is missing"),
        "{child}"
    );
    assert!(child.contains("Starting fresh."), "{child}");
    assert!(!child.contains("Earlier parent evidence."));
}

#[test]
fn task_stop_stops_a_hosted_fork_child() {
    assert_hosted_fork_stop(false);
}

#[test]
fn stopping_a_hosted_fork_does_not_cancel_its_parent_or_siblings() {
    assert_hosted_fork_stop(true);
}

fn assert_hosted_fork_stop(with_sibling: bool) {
    use crate::agent_controller::AgentController;
    use crate::runtime_host::RuntimeHost;
    use crate::runtime_subagent_call::RuntimeSubagentInvocation;
    use crate::runtime_tool_actor::RuntimeToolActorContext;
    use crate::runtime_tool_call::RuntimeToolCallRuntime;
    use crate::subagent::create_subagent_request;
    use crate::tasks::TaskRegistry;
    use orca_core::conversation::ProviderForkPrefix;
    use orca_core::task_types::TaskStatus;
    use std::time::{Duration, Instant};

    let cwd = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::history::redirect_test_orca_home(home.path());
    let registry = TaskRegistry::new("hosted-fork-stop".into());
    let parent = registry.create_main_session("fork parent".into());
    registry.mark_running(&parent.id).unwrap();
    let parent_cancel = registry.get(&parent.id).unwrap().control.cancel;
    let mut config = config(Some(FLASH_MODEL));
    config.cwd = Some(cwd.path().to_path_buf());
    config.provider = ProviderKind::DeepSeek;
    let endpoint = "http://127.0.0.1:0";
    config.base_url = Some(endpoint.into());
    let prefix_ref = registry
        .continuation_store()
        .unwrap()
        .store_fork_prefix(ProviderForkPrefix {
            model: FLASH_MODEL.into(),
            endpoint: endpoint.into(),
            reasoning_effort: config.reasoning_effort,
            messages: vec![r#"{"role":"user","content":"Earlier parent evidence."}"#.into()],
            tools: vec![],
            estimated_tokens: 5,
            system_messages: vec![],
            redacted: false,
        })
        .unwrap();

    let host = RuntimeHost::start().unwrap();
    let thread = host.start_thread(config.clone(), "Fork parent").unwrap();
    let controller = Arc::new(AgentController::new(
        host.handle(),
        thread.thread_id().into(),
        thread.thread_id().into(),
        0,
    ));
    let runtime = RuntimeToolCallRuntime::for_normal_execution();
    let mut batch = runtime.start_subagent_batch(
        &parent_cancel,
        &registry,
        config.subagents.limits,
        None,
        Some(&parent.id),
    );
    let batch_size = if with_sibling { 2 } else { 1 };
    if with_sibling {
        std::fs::write(cwd.path().join("require-sibling"), []).unwrap();
    }
    for (index, name) in ["stop-target", "sibling"]
        .into_iter()
        .take(batch_size)
        .enumerate()
    {
        let mut tool_request = fork_tool_request(Some("fork"));
        tool_request.id = name.into();
        let mut request = create_subagent_request(&tool_request);
        request.description = name.into();
        request.prompt = name.into();
        request.fork_prefix_ref = Some(prefix_ref);
        let invocation = RuntimeSubagentInvocation::snapshot(
            tool_request,
            request,
            &config,
            cwd.path(),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            None,
            1,
            |_, request, runtime, _| {
                assert!(request.fork_prefix.is_some());
                std::fs::write(runtime.cwd.join(&request.prompt), [])?;
                let release_sibling = runtime.cwd.join("release-sibling");
                let is_target = request.prompt == "stop-target";
                std::fs::write(runtime.cwd.join(format!("ready-{}", request.prompt)), [])?;
                let readiness_deadline = Instant::now() + Duration::from_secs(10);
                while runtime.cwd.join("require-sibling").exists()
                    && (!runtime.cwd.join("ready-stop-target").exists()
                        || !runtime.cwd.join("ready-sibling").exists())
                {
                    if Instant::now() >= readiness_deadline {
                        return Err(io::Error::other(
                            "sibling did not reach the readiness barrier",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let deadline = Instant::now() + Duration::from_secs(if is_target { 3 } else { 10 });
                while !runtime.cancel.is_cancelled() && (is_target || !release_sibling.exists()) {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("fork did not observe the stop or release"));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(ChildAgentResult {
                    status: if runtime.cancel.is_cancelled() {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Success
                    },
                    final_message: Some("fork finished".into()),
                    error: None,
                    budget_usage: None,
                })
            },
            None,
            None,
            &registry,
            Some(&parent.id),
            Some(Arc::clone(&controller)),
            "fork-stop-batch".into(),
            batch_size as u32,
        );
        let admission = batch.admit(index, invocation, |_| Ok(()));
        assert!(admission.immediate.is_none());
        assert!(admission.event_error.is_none());
    }

    let wait_for = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for a fork");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let target = cwd.path().join("stop-target");
    wait_for(&|| target.exists() && (!with_sibling || cwd.path().join("sibling").exists()));
    let task_id = registry
        .list()
        .into_iter()
        .find(|task| task.description == "stop-target")
        .unwrap()
        .id;
    let stop = RuntimeToolActorContext::new("fork-parent").execute_task_stop_tool(
        &ToolRequest {
            id: "stop-fork".into(),
            name: ToolName::TaskStop,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(serde_json::json!({"task_id": task_id}).to_string()),
        },
        &registry,
    );
    assert_eq!(stop.status, ToolStatus::Completed, "{:?}", stop.error);
    wait_for(&|| !registry.get(&task_id).unwrap().status.is_active());
    std::fs::write(cwd.path().join("release-sibling"), []).unwrap();
    let (mut outputs, event_error) = batch.finish(&mut |_, _, _, _| Ok(()));
    host.shutdown().unwrap();

    assert!(event_error.is_none());
    outputs.sort_by_key(|(index, _)| *index);
    let stopped = &outputs[0].1;
    assert_eq!(
        stopped.status,
        RunStatus::Cancelled,
        "{:?}",
        stopped.event_error
    );
    assert_eq!(registry.get(&task_id).unwrap().status, TaskStatus::Stopped);
    if with_sibling {
        let sibling = &outputs[1].1;
        assert_eq!(
            sibling.status,
            RunStatus::Success,
            "{:?}",
            sibling.event_error
        );
        assert!(!parent_cancel.is_cancelled());
        assert_eq!(
            registry.get(&parent.id).unwrap().status,
            TaskStatus::Running
        );
    }
}

fn assert_hosted_fork_inherits(parent: &[u8], child: &[u8]) {
    let parent = recorded_wire_messages(parent);
    let child = recorded_wire_messages(child);
    let cut = parent
        .iter()
        .position(|message| message.get().contains("Active parent turn."))
        .unwrap();
    assert!(cut < child.len(), "first child request: {child:?}");
    for (expected, inherited) in parent[..cut].iter().zip(&child[..cut]) {
        assert_eq!(inherited.get(), expected.get());
    }
    assert!(child[cut].get().contains("Report the relevant evidence."));
}

fn recorded_hosted_fork(
    restart: bool,
    remove_prefix: bool,
    lease_duration_ms: Option<i64>,
) -> (Vec<u8>, Vec<u8>) {
    use crate::agent_controller::AgentController;
    use crate::runtime_host::RuntimeHost;
    use crate::runtime_subagent_call::{
        RuntimeSubagentInvocation, recover_queued_subagent_launches,
    };
    use crate::runtime_tool_call::RuntimeToolCallRuntime;
    use crate::subagent::{SubagentMode, create_subagent_request, prepare_fork_request};
    use crate::tasks::{QueuedSubagentLaunchIntent, TaskRegistry};
    use orca_core::config::DelegationSnapshot;
    use orca_core::conversation::Conversation;
    use orca_core::task_types::TaskStatus;
    use serde_json::json;
    use std::time::{Duration, Instant};

    let cwd = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = crate::history::redirect_test_orca_home(home.path());
    let tasks = home.path().join("tasks");
    let session = "hosted-fork";
    let mut registry = TaskRegistry::new_persistent(session.into(), tasks.clone()).unwrap();
    registry.continuation_lease_duration_ms = lease_duration_ms;
    let (base_url, server) = recorded_provider(
        vec![
            json!({"content": "parent reply"}),
            json!({"content": "child reply"}),
        ],
        move |index, _, _| {
            if index == 1
                && let Some(lease_duration_ms) = lease_duration_ms
            {
                std::thread::sleep(Duration::from_millis(lease_duration_ms as u64 + 200));
            }
        },
    );
    let mut config = config(Some(PRO_MODEL));
    config.cwd = Some(cwd.path().to_path_buf());
    config.provider = ProviderKind::DeepSeek;
    config.approval_mode = ApprovalMode::FullAuto;
    config.api_key = Some("local-fixture".into());
    config.base_url = Some(base_url);
    let mut parent = Conversation::new();
    parent.add_system("Hosted parent system prompt.".into());
    parent.add_user("Earlier parent evidence.".into());
    parent.add_assistant(Some("Earlier parent report.".into()), None, vec![]);
    parent.add_user("Active parent turn.".into());
    parent.start_provider_turn("hosted-parent-turn");
    let cancel = CancelToken::new();
    let recorded = orca_provider::call_streaming(
        ProviderKind::DeepSeek,
        &parent,
        &orca_provider::ProviderConfig {
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            model: Some(PRO_MODEL.into()),
            reasoning_effort: orca_core::config::ReasoningEffort::High,
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        },
        &cancel,
        &mut |_| {},
    );
    assert_eq!(recorded.assistant_content.as_deref(), Some("parent reply"));
    let tool_request = fork_tool_request(Some("fork"));
    let mut request = create_subagent_request(&tool_request);
    request.mode = SubagentMode::Sync;
    request.delegation = Some(DelegationSnapshot::from_config(&config));
    assert_eq!(
        prepare_fork_request(&mut request, &config, Some(&parent), &registry).unwrap(),
        None
    );
    assert!(request.fork_prefix_ref.is_some());
    if remove_prefix {
        let prefix_dir = tasks.join(session).join("fork-prefixes");
        let mut entries = std::fs::read_dir(prefix_dir).unwrap();
        std::fs::remove_file(entries.next().unwrap().unwrap().path()).unwrap();
        assert!(entries.next().is_none());
    }

    let host = RuntimeHost::start().unwrap();
    let thread = host.start_thread(config.clone(), "Fork parent").unwrap();
    let controller = Arc::new(AgentController::new(
        host.handle(),
        thread.thread_id().into(),
        thread.thread_id().into(),
        0,
    ));
    let mcp = McpRegistry::default();
    let hooks = HookRunner::default();
    let executor = crate::agent_loop::execute_child_agent_loop;
    if restart {
        let submission = registry
            .execution_scope(&config.subagents.limits)
            .submit_subagent_with_intent(
                request.description.clone(),
                Some("explorer".into()),
                None,
                0,
                Some(QueuedSubagentLaunchIntent {
                    schema_version: QueuedSubagentLaunchIntent::SCHEMA_VERSION,
                    tool_request,
                    request,
                    cwd: cwd.path().to_path_buf(),
                    child_depth: 1,
                    batch_id: "fork-batch".into(),
                    batch_size: 1,
                }),
                false,
            )
            .unwrap();
        drop(registry);
        let registry = TaskRegistry::new_persistent(session.into(), tasks).unwrap();
        let queued = registry.queued_subagent_launches(None).unwrap();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].1.request.captured_fork_prefix.is_none());
        assert_eq!(
            recover_queued_subagent_launches(
                &config,
                &mcp,
                &hooks,
                executor,
                None,
                None,
                &registry,
                None,
                Some(controller),
            )
            .unwrap(),
            1
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let task = registry.get(&submission.task_id).unwrap();
            if !task.status.is_active() {
                assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.error);
                break;
            }
            assert!(Instant::now() < deadline, "recovered fork did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        let invocation = RuntimeSubagentInvocation::snapshot(
            tool_request,
            request,
            &config,
            cwd.path(),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &mcp,
            &hooks,
            None,
            1,
            executor,
            None,
            None,
            &registry,
            None,
            Some(controller),
            "fork-batch".into(),
            1,
        );
        let result = RuntimeToolCallRuntime::for_normal_execution().execute_subagent(
            invocation,
            &cancel,
            |_| Ok(()),
        );
        assert_eq!(
            result.output.status,
            RunStatus::Success,
            "{:?}",
            result.output.result.error
        );
    }
    host.shutdown().unwrap();
    let [parent, child] = server
        .join()
        .unwrap()
        .try_into()
        .expect("parent and first child requests");
    (parent, child)
}

#[test]
fn the_in_flight_turn_is_not_inherited() {
    let (bodies, _) = run_recorded_fork(Some("fork"), RecordedHistory::Ordinary, false);
    let child = std::str::from_utf8(&bodies[1]).unwrap();
    assert!(child.contains("parent completed report"));
    for marker in ["in-flight", "active-tool", "pending-tool"] {
        assert!(!child.contains(marker), "child inherited {marker}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ForkResumeChange {
    None,
    InstructionsChanged,
    TrustRevoked,
}

#[test]
fn a_resumed_fork_child_keeps_its_prefix() {
    assert_dispatched_fork_resume(ForkResumeChange::None);
    assert_dispatched_fork_resume(ForkResumeChange::InstructionsChanged);
}

#[test]
fn a_fork_child_drops_inherited_instructions_whose_trust_was_revoked() {
    assert_dispatched_fork_resume(ForkResumeChange::TrustRevoked);
}

fn assert_dispatched_fork_resume(change: ForkResumeChange) {
    use crate::lifecycle::{AgentLoopContext, AgentLoopOutcome};
    use crate::runtime_conversation_bootstrap::AgentConversationContext;
    use crate::tasks::TaskRegistry;
    use crate::tool_invocation::AgentToolPolicyContext;
    use orca_core::config::folder_trust::{TrustLevel, set_trust};
    use orca_core::conversation::Conversation;
    use orca_core::task_types::TaskStatus;
    use serde_json::{Value, json};

    let cwd = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("sibling.txt"), "active sibling evidence").unwrap();
    let instruction_path = cwd.path().join("AGENTS.md");
    std::fs::write(&instruction_path, "Follow the original workspace runbook.").unwrap();
    set_trust(cwd.path(), TrustLevel::Trusted).unwrap();
    let registry =
        TaskRegistry::new_persistent("fork-dispatch".into(), home.path().join("task-sessions"))
            .unwrap();
    let parent = registry.create_main_session("fork dispatch parent".into());
    registry.mark_running(&parent.id).unwrap();
    let blocker = registry.create_subagent("occupy the child slot".into(), None);
    registry.mark_running(&blocker.id).unwrap();

    let tool = |index: usize, id: &str, name: &str, arguments: Value| {
        json!({"index": index, "id": id, "type": "function", "function": {
            "name": name, "arguments": arguments.to_string()
        }})
    };
    let mut replies = vec![
        json!({"content": "completed parent evidence"}),
        json!({"reasoning_content": "dispatch two calls", "tool_calls": [
            tool(0, "sibling-read", "read_file", json!({"path": "sibling.txt"})),
            tool(1, "fork-launch", "subagent", json!({
                "context": "fork", "mode": "async", "subagent_type": "explorer",
                "description": "queued fork child", "prompt": "Report the completed evidence."
            }))
        ]}),
        json!({"content": "waiting for the fork report"}),
        json!({"content": "initial fork child report"}),
        json!({"content": "integrated the fork report"}),
        json!({"reasoning_content": "resume the child", "tool_calls": [
            tool(0, "fork-resume", "subagent", json!({}))
        ]}),
        json!({"content": "resumed fork child report"}),
        json!({"content": "parent resume report"}),
    ];
    if change == ForkResumeChange::InstructionsChanged {
        replies.extend([
            json!({"reasoning_content": "resume again", "tool_calls": [
                tool(0, "fork-resume-again", "subagent", json!({}))
            ]}),
            json!({"content": "second resumed child report"}),
            json!({"content": "second parent resume report"}),
        ]);
    }
    let mut fork_task = None;
    let worker_registry = registry.clone();
    let parent_id = parent.id.clone();
    let mut first_resume_source = None;
    let (base_url, server) = recorded_provider(replies, move |index, body, delta| match index {
        2 => {
            let request: Value = serde_json::from_slice(body).unwrap();
            let messages = request["messages"].as_array().unwrap();
            let tool_result = |id: &str| {
                messages
                    .iter()
                    .find(|message| message["tool_call_id"] == id)
                    .unwrap()["content"]
                    .as_str()
                    .unwrap()
            };
            assert!(tool_result("sibling-read").contains("active sibling evidence"));
            let launch: Value = serde_json::from_str(tool_result("fork-launch")).unwrap();
            assert_eq!(launch["status"], "queued");
            let task_id = launch["task_id"].as_str().unwrap();
            let queued = worker_registry
                .queued_subagent_launches(Some(&parent_id))
                .unwrap();
            assert!(queued.iter().any(|(id, _)| id == task_id));
            fork_task = Some(task_id.to_owned());
            worker_registry
                .complete(&blocker.id, "slot released".into())
                .unwrap();
        }
        5 => {
            let task_id = fork_task.as_deref().unwrap();
            let task = worker_registry.get(task_id).unwrap();
            assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.error);
            delta["tool_calls"][0]["function"]["arguments"] = json!({
                "description": "resume the fork child", "prompt": "Continue your report.",
                "resume_from": task_id, "mode": "sync"
            })
            .to_string()
            .into();
        }
        7 if change == ForkResumeChange::InstructionsChanged => {
            let request: Value = serde_json::from_slice(body).unwrap();
            let resume = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["tool_call_id"] == "fork-resume")
                .unwrap();
            first_resume_source = resume["content"]
                .as_str()
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("resume_from="))
                .map(str::to_owned);
        }
        8 => {
            let task_id = first_resume_source.as_deref().unwrap();
            delta["tool_calls"][0]["function"]["arguments"] = json!({
                "description": "resume again", "prompt": "Add a final detail.",
                "resume_from": task_id, "mode": "sync"
            })
            .to_string()
            .into();
        }
        _ => {}
    });
    let mut config = config(Some(PRO_MODEL));
    config.provider = ProviderKind::DeepSeek;
    config.approval_mode = ApprovalMode::FullAuto;
    config.api_key = Some("local-fixture".into());
    config.base_url = Some(base_url);
    config.subagents.limits.max_running = 1;

    let mut conversation = Conversation::new();
    let parent_instructions = crate::instructions::load_for_cwd(cwd.path()).unwrap();
    conversation.messages.push(
        crate::agent_common::build_agent_system_prompt_sections(
            cwd.path(),
            &config,
            0,
            &SubagentType::General,
            Some(&parent_instructions),
            None,
            None,
        )
        .into_message(),
    );
    let mut run_turn = |prompt: &str, expected: &str| {
        conversation.add_user(prompt.into());
        let outcome = crate::agent_loop::run_agent_loop(
            &config,
            AgentLoopContext::new(cwd.path(), prompt, 0, false, &SubagentType::General)
                .with_root_task_id(Some(&parent.id))
                .with_services(
                    &ProjectInstructions::default(),
                    &MemoryBlock::default(),
                    &McpRegistry::default(),
                    &HookRunner::default(),
                )
                .with_runtime(&mut CostTracker::new(None), &CancelToken::new(), &registry)
                .with_execution(&mut Vec::new(), None, None),
            &mut EventFactory::new("fork-dispatch".into()),
            &mut EventSink::new(io::sink(), OutputFormat::Jsonl),
            AgentConversationContext::borrowed(&mut conversation, None, false),
            AgentToolPolicyContext::new(None, None),
        )
        .unwrap();
        let AgentLoopOutcome::Completed(result) = outcome else {
            panic!("parent turn did not complete");
        };
        assert_eq!(
            result.status,
            RunStatus::Success,
            "{prompt}: {:?}",
            result.error
        );
        assert_eq!(result.final_message.as_deref(), Some(expected));
    };
    run_turn("Gather earlier evidence.", "completed parent evidence");
    run_turn(
        "Active parent turn: dispatch the fork.",
        "integrated the fork report",
    );
    match change {
        ForkResumeChange::None => {}
        ForkResumeChange::InstructionsChanged => {
            std::fs::write(&instruction_path, "Follow the updated workspace runbook.").unwrap();
        }
        ForkResumeChange::TrustRevoked => {
            set_trust(cwd.path(), TrustLevel::Untrusted).unwrap();
        }
    }
    run_turn("Resume the fork.", "parent resume report");

    if change == ForkResumeChange::InstructionsChanged {
        run_turn("Resume again.", "second parent resume report");
    }
    let bodies = server.join().unwrap();
    assert_eq!(
        bodies.len(),
        if change == ForkResumeChange::InstructionsChanged {
            11
        } else {
            8
        }
    );
    if change == ForkResumeChange::InstructionsChanged {
        let request = recorded_wire_messages(&bodies[9]);
        let update_positions: Vec<_> = request
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message
                    .get()
                    .contains("Follow the updated workspace runbook.")
            })
            .map(|(index, _)| index)
            .collect();
        let first_resume = request
            .iter()
            .position(|message| message.get().contains("Continue your report."))
            .unwrap();
        assert_eq!(update_positions.len(), 1);
        assert!(
            update_positions[0] < first_resume,
            "the second resume moved the previous notice"
        );
    }
    let parent = recorded_wire_messages(&bodies[1]);
    let cut = parent
        .iter()
        .position(|message| message.get().contains("Active parent turn"))
        .unwrap();
    for index in [3, 6] {
        let child = recorded_wire_messages(&bodies[index]);
        let body = std::str::from_utf8(&bodies[index]).unwrap();
        if index == 6 && change == ForkResumeChange::TrustRevoked {
            assert!(!body.contains("completed parent evidence"));
            assert!(!body.contains("Follow the original workspace runbook."));
            assert!(body.contains("inherited instruction sources no longer apply"));
            assert!(child[0].get().contains("read-only"));
        } else {
            assert!(body.contains("completed parent evidence"));
            assert!(body.contains("Follow the original workspace runbook."));
            for (parent, inherited) in parent[..cut].iter().zip(&child[..cut]) {
                assert_eq!(parent.get(), inherited.get());
            }
        }
        let updates: Vec<_> = child
            .iter()
            .filter(|message| {
                message
                    .get()
                    .contains("instructions changed; they now read")
            })
            .collect();
        if index == 6 && change == ForkResumeChange::InstructionsChanged {
            assert_eq!(updates.len(), 1);
            assert!(
                updates[0]
                    .get()
                    .contains("Follow the updated workspace runbook.")
            );
        } else {
            assert!(updates.is_empty());
        }
        for marker in [
            "Active parent turn",
            "sibling-read",
            "active sibling evidence",
            "fork-launch",
        ] {
            assert!(!body.contains(marker), "child inherited {marker}");
        }
        assert_eq!(body.contains("initial fork child report"), index == 6);
    }
}

#[test]
fn a_fork_childs_role_limits_are_enforced_at_execution() {
    let (bodies, result) = run_recorded_fork(Some("fork"), RecordedHistory::Ordinary, true);
    let first: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert!(
        first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "write_file")
    );
    assert_eq!(result.status, RunStatus::Success, "{:?}", result.error);
    let continued: serde_json::Value = serde_json::from_slice(&bodies[2]).unwrap();
    let call = continued["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .find(|call| call["id"] == "child-write")
        .unwrap();
    let arguments: serde_json::Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments["path"], "fork-write.txt");
    assert!(
        continued
            .to_string()
            .contains("role 'explorer' tool policy disallows tool 'write_file'")
    );
    assert_eq!(continued["tools"], first["tools"]);
    assert_eq!(result.final_message.as_deref(), Some("child report"));
}

#[test]
fn a_fork_childs_final_report_request_sends_no_tools() {
    let (bodies, result) = run_recorded_fork_with(
        Some("fork"),
        RecordedHistory::Ordinary,
        vec![
            serde_json::json!({"reasoning_content":"read the evidence", "tool_calls":[{
                "index":0,"id":"child-read","function":{"name":"read_file", "arguments":"{\"path\":\"evidence.txt\"}"}
            }]}),
        ],
        |cwd, config| {
            std::fs::write(cwd.join("evidence.txt"), "child evidence").unwrap();
            config.subagents.max_investigation_tool_calls = 1;
        },
    );
    assert_eq!(result.status, RunStatus::Success, "{:?}", result.error);
    let report: serde_json::Value = serde_json::from_slice(&bodies[2]).unwrap();
    assert!(
        report
            .to_string()
            .contains("Read-only investigation finalization")
    );
    assert!(report.to_string().contains("child evidence"));
    assert_eq!(report["tools"], serde_json::json!([]));
    assert!(report.to_string().contains("parent completed report"));
}

#[test]
fn a_fork_child_uses_the_parents_model() {
    let (bodies, _) = run_recorded_fork(Some("fork"), RecordedHistory::Ordinary, false);
    let child: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(child["model"], PRO_MODEL);
    assert_eq!(child["reasoning_effort"], "high");
}

#[test]
fn fresh_children_are_unchanged() {
    for context in [None, Some("fresh")] {
        let (bodies, _) = run_recorded_fork(context, RecordedHistory::Ordinary, false);
        let child: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
        assert_eq!(child["model"], FLASH_MODEL);
        assert_eq!(child["messages"][0]["role"], "system");
        assert!(
            child["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
        assert_eq!(
            child["messages"][1]["content"],
            "Report the relevant evidence."
        );
        assert!(
            !std::str::from_utf8(&bodies[1])
                .unwrap()
                .contains("Recorded parent system prompt")
        );
    }
}

#[test]
fn a_fork_before_any_completed_turn_starts_fresh_with_a_note() {
    let (bodies, _) = run_recorded_fork(Some("fork"), RecordedHistory::Empty, false);
    let child: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(child["model"], FLASH_MODEL);
    let prompt = child["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains("no completed parent turn"), "{prompt}");
    assert!(prompt.contains("Report the relevant evidence"));
}

#[test]
fn the_subagent_schema_exposes_fork_with_a_fresh_default() {
    let registry = orca_tools::registry::default_tool_registry();
    let context = &registry.get("subagent").unwrap().spec().input_schema["properties"]["context"];
    assert_eq!(context["enum"], serde_json::json!(["fresh", "fork"]));
    assert_eq!(context["default"], "fresh");
}

fn assert_recorded_history_is_inherited(history: RecordedHistory, marker: &str) {
    let (bodies, result) = run_recorded_fork(Some("fork"), history, false);
    let parent = recorded_wire_messages(&bodies[0]);
    let child = recorded_wire_messages(&bodies[1]);
    let inherited = parent
        .iter()
        .position(|message| message.get().contains("in-flight parent prompt"))
        .unwrap();
    assert!(
        parent[..inherited]
            .iter()
            .any(|message| message.get().contains(marker))
    );
    assert!(
        child.len() > inherited && child[0].get() == parent[0].get(),
        "child started fresh or lost history"
    );
    assert_eq!(
        child[..inherited]
            .iter()
            .map(|message| message.get())
            .collect::<Vec<_>>(),
        parent[..inherited]
            .iter()
            .map(|message| message.get())
            .collect::<Vec<_>>()
    );
    assert_eq!(result.status, RunStatus::Success);
}

#[test]
fn a_fork_after_opening_compaction_inherits_the_summary() {
    assert_recorded_history_is_inherited(RecordedHistory::OpeningCompaction, "[Summary baseline]");
}

#[test]
fn a_fork_after_an_image_rewrite_still_inherits() {
    assert_recorded_history_is_inherited(
        RecordedHistory::ImageRewrite,
        TOOL_IMAGE_UNAVAILABLE_NOTE,
    );
}

#[test]
fn an_interrupted_earlier_turn_is_inherited_as_the_parent_sent_it() {
    assert_recorded_history_is_inherited(
        RecordedHistory::Interrupted,
        "Describe the recorded evidence",
    );
}
