use orca_approval::ApprovalPolicy;
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest};
use orca_core::config::{
    ActivePermissionProfile, HistoryMode, ModelRuntimeConfig, OutputFormat,
    PermissionProfileNetworkAccess, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::{ModelSelection, PRO_MODEL};
use orca_core::provider_types::{ProviderStep, Usage};
use orca_core::subagent_config::SubagentConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{TaskStatus, TaskType};
use orca_core::thread_identity::TurnId;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::HookRunner;
use orca_runtime::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimePermissionRequest,
    RuntimePermissionRequestHandler, RuntimePermissionResponse, RuntimeSessionLifecycle,
    RuntimeSpecialToolDispatch, RuntimeSubagentStatusLookup, RuntimeSubagentStatusRecord,
    RuntimeTaskActor, RuntimeTaskKind, RuntimeTaskStatus, RuntimeToolActorContext,
    RuntimeTurnRunner, RuntimeUserInputHandler, RuntimeUserInputRequest,
    RuntimeWorkflowDraftRequest, RuntimeWorkflowIpc, TurnPermissionOverlay,
};
use orca_runtime::protocol::{
    PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
    RequestNetworkPermissions, RequestPermissionProfile,
};
use orca_runtime::runtime_permission::RuntimePermissionContext;
use orca_runtime::tasks::TaskRegistry;
use serde_json::Value;
use tempfile::tempdir;

fn hook_test_cwd() -> String {
    std::env::temp_dir().display().to_string()
}

#[path = "support/sandbox_test_parent.rs"]
mod sandbox_test_support;

use sandbox_test_support::sandbox_test_parent;

fn danger_full_access_config() -> RunConfig {
    let mut config = test_run_config();
    config.active_permission_profile = Some(ActivePermissionProfile {
        id: "danger-full-access".to_string(),
        extends: None,
    });
    config
}

#[test]
fn reported_session_triggers_soft_compaction() {
    // Exact JSONL bytes reproduce the released session incident contract.
    let fixture = include_str!("fixtures/session_stuck_2026_07_08.min.jsonl");
    let records = fixture
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture jsonl line"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["type"], "session.usage");
    assert_eq!(records[1]["type"], "conversation.message");
    assert_eq!(records[2]["type"], "conversation.message");

    let read_arguments = records[1]["message"]["tool_calls"][0]["arguments"]
        .as_str()
        .expect("read_file arguments");
    let read_args: Value = serde_json::from_str(read_arguments).expect("read args json");
    assert_eq!(read_args["path"], "lib/meta.ts");
    assert_eq!(read_args["offset"], 175);
    assert_eq!(read_args["limit"], 70);

    let cwd = tempdir().expect("temp workspace");
    std::fs::create_dir_all(cwd.path().join("lib")).expect("create lib dir");
    let contents = (1..=250)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(cwd.path().join("lib/meta.ts"), contents).expect("write synthetic file");
    let read_request = ToolRequest {
        id: "call_read".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("lib/meta.ts".to_string()),
        raw_arguments: Some(read_arguments.to_string()),
    };

    let read_result = orca_tools::read_file::execute(&read_request, cwd.path(), 100_000);
    let read_output = read_result.output.as_deref().expect("read output");
    let read_lines = read_output.lines().collect::<Vec<_>>();
    assert_eq!(read_lines.len(), 70);
    assert_eq!(read_lines[0], "175: line 175");
    assert_eq!(read_lines[69], "244: line 244");

    let mut with_reasoning = Conversation::new();
    with_reasoning.add_user("first".to_string());
    with_reasoning.add_assistant(
        Some("done".to_string()),
        Some("private reasoning ".repeat(20_000)),
        vec![],
    );
    with_reasoning.add_user("next".to_string());
    let mut without_reasoning = Conversation::new();
    without_reasoning.add_user("first".to_string());
    without_reasoning.add_assistant(Some("done".to_string()), None, vec![]);
    without_reasoning.add_user("next".to_string());
    assert_eq!(
        orca_provider::context::conversation_tokens(&with_reasoning),
        orca_provider::context::conversation_tokens(&without_reasoning)
    );

    let runtime = ModelRuntimeConfig {
        context_window: Some(1_000_000),
        auto_compact_token_limit: None,
        soft_compact_token_limit: Some(96_000),
    };
    let context_config =
        orca_provider::context::ContextConfig::for_model_with_runtime(Some(PRO_MODEL), &runtime);
    let pressure = orca_provider::context::context_pressure_for_tokens(120_000, &context_config);
    assert_eq!(pressure.soft_limit, 96_000);
    // Hard ceiling = 1_000_000 * 0.90 - 4096 = 895_904 (safety net); the soft
    // line above is an explicit absolute override.
    assert_eq!(pressure.effective_limit, 895_904);
    assert!(pressure.should_soft_compact);
    assert!(!pressure.should_hard_compact);
}

#[test]
fn session_lifecycle_assigns_agent_task_and_monotonic_turns() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    let task = lifecycle.start_task(RuntimeTaskKind::Agent);

    assert_eq!(task.id(), "run-test:task-1");
    assert_eq!(task.kind(), RuntimeTaskKind::Agent);
    assert_eq!(task.status(), RuntimeTaskStatus::Running);

    let first = lifecycle.next_turn();
    let second = lifecycle.next_turn();

    assert_eq!(first.number(), 1);
    assert_eq!(second.number(), 2);
    assert_eq!(lifecycle.active_task().unwrap().current_turn(), 2);
}

#[test]
fn turn_started_event_carries_task_lifecycle_payload() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let turn = lifecycle.next_turn();
    let task = lifecycle.active_task().unwrap();
    let mut events = EventFactory::new(lifecycle.run_id().to_string());
    let turn_id = TurnId::new();

    let event = turn.started_event(&mut events, &turn_id, Some("hello"), task);

    assert_eq!(event.payload["turn_id"], turn_id.as_str());
    assert_eq!(event.payload["turn"], 1);
    assert_eq!(event.payload["prompt"], "hello");
    assert_eq!(event.payload["task"]["task_id"], "run-test:task-1");
    assert_eq!(event.payload["task"]["kind"], "agent");
    assert_eq!(event.payload["task"]["status"], "running");
}

#[test]
fn turn_runner_advances_lifecycle_and_builds_started_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut events = EventFactory::new(lifecycle.run_id().to_string());
    let mut runner = RuntimeTurnRunner::new(&mut lifecycle);
    let turn_id = TurnId::new();

    let started = runner.start_turn(&mut events, &turn_id, Some("hello"));

    assert_eq!(started.turn(), 1);
    assert_eq!(started.event.payload["turn_id"], turn_id.as_str());
    assert_eq!(started.event.payload["turn"], 1);
    assert_eq!(started.event.payload["prompt"], "hello");
    assert_eq!(started.event.payload["task"]["kind"], "agent");
    assert_eq!(started.event.payload["task"]["status"], "running");
    assert_eq!(started.event.payload["task"]["turn"], 1);
}

#[test]
fn turn_runner_advances_lifecycle_without_emitting_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut runner = RuntimeTurnRunner::new(&mut lifecycle);

    let advanced = runner.advance_turn();

    assert_eq!(advanced.turn(), 1);
    let task = advanced.task().expect("task snapshot");
    assert_eq!(task.kind(), RuntimeTaskKind::Subagent);
    assert_eq!(task.current_turn(), 1);
}

#[test]
fn finish_task_maps_run_status_to_lifecycle_status() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);

    let task = lifecycle.finish_task(RunStatus::Failed).unwrap();

    assert_eq!(task.status(), RuntimeTaskStatus::Failed);
    assert_eq!(task.payload()["status"], "failed");
}

#[test]
fn shell_task_snapshot_serializes_lifecycle_payload() {
    let task = orca_runtime::lifecycle::RuntimeTaskLifecycle::new_snapshot(
        "shell-call-1:task-1",
        RuntimeTaskKind::Shell,
        RuntimeTaskStatus::Succeeded,
        1,
    );

    assert_eq!(task.payload()["task_id"], "shell-call-1:task-1");
    assert_eq!(task.payload()["kind"], "shell");
    assert_eq!(task.payload()["status"], "succeeded");
    assert_eq!(task.payload()["turn"], 1);
}

#[test]
fn task_actor_starts_turns_without_implicit_ceiling() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut events = EventFactory::new("run-actor".to_string());
    let turn_id = TurnId::new();

    // The hidden 128-turn ceiling is gone: an unbounded actor admits turns
    // far beyond the legacy limit; explicit limits live in BudgetController.
    for turn in 1..=150 {
        let started = actor
            .start_turn(&mut events, &turn_id, Some("hello"), true)
            .expect("unlimited turns start");
        assert_eq!(started.turn(), turn);
    }
    let started = actor
        .start_turn(&mut events, &turn_id, Some("hello"), true)
        .expect("151st turn starts");
    let event = started.event().expect("emitted event");
    assert_eq!(event.payload["turn"], 151);
    assert_eq!(event.payload["task"]["task_id"], "run-actor:task-1");
    assert_eq!(event.payload["task"]["kind"], "agent");
}

#[test]
fn task_actor_starts_turn_from_existing_lifecycle_state() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    lifecycle.next_turn();
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut events = EventFactory::new("run-actor".to_string());
    let turn_id = TurnId::new();

    let started = actor
        .start_turn(&mut events, &turn_id, Some("second turn"), true)
        .expect("turn starts regardless of prior lifecycle state");

    assert_eq!(started.turn(), 2);
    assert_eq!(actor.active_task().expect("task").current_turn(), 2);
}

#[test]
fn task_actor_advances_turn_without_emitting_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut events = EventFactory::new("run-actor".to_string());
    let turn_id = TurnId::new();

    let first = actor
        .start_turn(&mut events, &turn_id, Some("hello"), false)
        .expect("first turn");

    assert_eq!(first.turn(), 1);
    assert!(first.event().is_none());
    assert_eq!(actor.active_task().expect("task").payload()["turn"], 1);
}

#[test]
fn task_actor_routes_model_turn_and_updates_cost_model() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut cost_tracker = CostTracker::new(Some("deepseek-v4-flash"));
    let provider_config = ProviderConfig {
        api_key: None,
        base_url: None,
        model: None,
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
    };

    let routed = actor.route_model_turn(
        &ModelSelection::from_unchecked(Some("auto".to_string())),
        &SubagentType::General,
        None,
        false,
        &provider_config,
        &mut cost_tracker,
    );

    assert_eq!(routed.decision.actual_model, PRO_MODEL);
    assert_eq!(routed.provider_config.model.as_deref(), Some(PRO_MODEL));
    let totals = cost_tracker.add_usage(Usage {
        input_tokens: 100,
        output_tokens: 50,
        cache_tokens: 0,
    });
    let expected_pro_cost = (100.0 * 0.435 + 50.0 * 0.87) / 1_000_000.0;
    assert!((totals.estimated_cost_usd - expected_pro_cost).abs() < 1e-12);
}

#[test]
fn task_actor_records_usage_and_reports_budget_exhaustion() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut cost_tracker = CostTracker::new(Some(PRO_MODEL));

    let exhausted = actor
        .record_usage(
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_tokens: 0,
            },
            &mut cost_tracker,
            Some(1),
        )
        .expect_err("budget exhausted");

    assert_eq!(exhausted.status, RunStatus::Failed);
    assert!(exhausted.message.contains("budget stopped"));
    assert_eq!(cost_tracker.totals().total_tokens(), 2_000_000);
}

#[test]
fn task_actor_runs_pre_model_hook_and_returns_injected_context() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "printf '%s' '{\"action\":\"inject\",\"context\":\"actor hook context\"}'"
            .to_string(),
        tool: None,
    }]);

    let outcome = actor
        .run_pre_model_hook(&hooks, &hook_test_cwd())
        .expect("pre model hook");

    assert_eq!(outcome.injected_context, vec!["actor hook context"]);
}

#[test]
fn task_actor_formats_post_model_hook_failure_as_warning() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PostModelCall,
        command: "exit 7".to_string(),
        tool: None,
    }]);

    let warning = actor
        .run_post_model_hook(
            &hooks,
            &hook_test_cwd(),
            Some(&Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_tokens: 0,
            }),
        )
        .expect("post model warning");

    assert!(warning.contains("post_model_call hook failed"));
}

#[test]
fn task_actor_calls_streaming_provider_and_forwards_model_deltas() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let provider_config = ProviderConfig {
        api_key: None,
        base_url: None,
        model: None,
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let mut conversation = Conversation::new();
    conversation.add_user("mock_usage".to_string());
    let cancel = orca_core::cancel::CancelToken::new();
    let mut streamed = Vec::new();

    let response = actor.call_streaming_provider(
        ProviderKind::Mock,
        &conversation,
        &provider_config,
        &cancel,
        &mut |step| match step {
            ProviderStep::ReasoningDelta(text) | ProviderStep::MessageDelta(text) => {
                streamed.push(text.clone())
            }
            _ => {}
        },
    );

    assert_eq!(
        streamed,
        vec![
            "Mock runtime is preserving the DeepSeek reasoning channel.".to_string(),
            "Mock runtime completed with usage accounting.".to_string(),
        ]
    );
    assert_eq!(
        response.assistant_content.as_deref(),
        Some("Mock runtime completed with usage accounting.")
    );
    let usage = response.usage.expect("usage");
    assert_eq!(
        usage.input_tokens + usage.output_tokens + usage.cache_tokens,
        160
    );
}

#[test]
fn task_actor_builds_shell_tool_events_with_task_lifecycle() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut events = EventFactory::new("run-actor".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };

    let requested = actor.tool_call_requested_event(&mut events, &request);

    assert_eq!(requested.payload["task"]["task_id"], "shell-tool-1:task-1");
    assert_eq!(requested.payload["task"]["kind"], "shell");
    assert_eq!(requested.payload["task"]["status"], "running");
    assert_eq!(requested.payload["task"]["turn"], 1);

    let result = ToolResult::completed(&request, "hi".to_string(), false);
    let completed = actor.tool_call_completed_event(&mut events, &request, &result);

    assert_eq!(completed.payload["task"]["task_id"], "shell-tool-1:task-1");
    assert_eq!(completed.payload["task"]["kind"], "shell");
    assert_eq!(completed.payload["task"]["status"], "succeeded");
    assert_eq!(completed.payload["task"]["turn"], 1);
}

#[test]
fn task_actor_runs_pre_tool_hook_and_formats_blocked_result() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreToolUse,
        command: "printf '%s' '{\"action\":\"deny\",\"reason\":\"blocked by actor\"}'".to_string(),
        tool: None,
    }]);

    let blocked = actor
        .run_pre_tool_hook(&hooks, &hook_test_cwd(), &request)
        .expect_err("blocked result");

    assert_eq!(blocked.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        blocked.error.as_deref(),
        Some("pre_tool_use hook blocked tool: blocked by actor")
    );
}

#[test]
fn task_actor_formats_post_tool_hook_failure_as_warning() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let result = ToolResult::completed(&request, "hi".to_string(), false);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PostToolUse,
        command: "exit 9".to_string(),
        tool: None,
    }]);

    let warning = actor
        .run_post_tool_hook(&hooks, &hook_test_cwd(), &request, &result)
        .expect("post tool warning");

    assert!(warning.contains("post_tool_use hook failed"));
}

#[test]
fn task_actor_resolves_required_tool_approval_as_allowed() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let decision = actor.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::FullAuto),
        Some(approval),
        &request,
    );

    match decision {
        RuntimeApprovalDecision::Allowed(resolution) => {
            assert_eq!(
                resolution.decision,
                orca_core::approval_types::ApprovalDecision::Allow
            );
            assert_eq!(resolution.reason, "full-auto permits shell");
        }
        other => panic!("expected allowed approval decision, got {other:?}"),
    }
}

#[test]
fn task_actor_resolves_denied_tool_approval_with_denied_result() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let decision = actor.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::Plan),
        Some(approval),
        &request,
    );

    match decision {
        RuntimeApprovalDecision::Denied { resolution, result } => {
            assert_eq!(
                resolution.decision,
                orca_core::approval_types::ApprovalDecision::Deny
            );
            assert_eq!(resolution.reason, "plan denies shell");
            assert_eq!(result.status, orca_core::tool_types::ToolStatus::Denied);
            assert_eq!(result.error.as_deref(), Some("plan denies shell"));
        }
        other => panic!("expected denied approval decision, got {other:?}"),
    }
}

#[test]
fn task_actor_routes_interactive_approval_through_handler() {
    struct DenyHandler;

    impl RuntimeApprovalHandler for DenyHandler {
        fn resolve_interactive(
            &self,
            approval: &ApprovalRequest,
            _request: &ToolRequest,
        ) -> std::io::Result<orca_core::approval_types::ApprovalResolution> {
            Ok(orca_core::approval_types::ApprovalResolution {
                id: approval.id.clone(),
                decision: ApprovalDecision::Deny,
                reason: "handler denied".to_string(),
            })
        }
    }

    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let resolution = actor
        .resolve_interactive_tool_approval(&DenyHandler, &approval, &request)
        .expect("interactive approval resolution");

    assert_eq!(resolution.id, "approval-tool-1");
    assert_eq!(resolution.decision, ApprovalDecision::Deny);
    assert_eq!(resolution.reason, "handler denied");
}

#[test]
fn tool_actor_context_routes_canonical_user_question_through_handler() {
    struct AnswerHandler;

    impl RuntimeUserInputHandler for AnswerHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            assert_eq!(request.id, "ask:question:1");
            assert_eq!(request.question, "Confirm: Continue?");
            assert_eq!(
                request.choices,
                vec!["yes - Continue".to_string(), "no - Stop".to_string()]
            );
            Ok(Some("yes".to_string()))
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "ask".to_string(),
        name: ToolName::AskUserQuestion,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(
            r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#
                .to_string(),
        ),
    };

    let result = context
        .execute_user_input_tool(&request, &AnswerHandler)
        .expect("user input result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        result.output.as_deref(),
        Some(r#"{"answers":{"Continue?":"yes"}}"#)
    );
}

#[test]
fn tool_actor_context_cancelled_user_input_returns_cancelled_result() {
    struct CancelHandler;

    impl RuntimeUserInputHandler for CancelHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            assert_eq!(request.id, "ask:question:1");
            Ok(None)
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "ask".to_string(),
        name: ToolName::AskUserQuestion,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(
            r#"{"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"yes","description":"Continue"},{"label":"no","description":"Stop"}]}]}"#
                .to_string(),
        ),
    };

    let result = context
        .execute_user_input_tool(&request, &CancelHandler)
        .expect("user input result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Cancelled);
    assert_eq!(
        result.kind,
        orca_core::tool_types::ToolResultKind::Cancelled
    );
    assert_eq!(
        result.error.as_deref(),
        Some("user question request cancelled")
    );
}

#[test]
fn tool_actor_context_maps_invalid_ask_user_question_to_invalid_input_result() {
    struct UnexpectedHandler;

    impl RuntimeUserInputHandler for UnexpectedHandler {
        fn request_user_input(
            &self,
            _request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            panic!("invalid questionnaire must not reach the interaction handler")
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "ask-invalid".to_string(),
        name: ToolName::AskUserQuestion,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(r#"{"questions":[]}"#.to_string()),
    };

    let result = context
        .execute_user_input_tool(&request, &UnexpectedHandler)
        .expect("invalid questionnaire is a tool result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.kind,
        orca_core::tool_types::ToolResultKind::InvalidInput
    );
    assert_eq!(
        result.terminal().started,
        orca_core::tool_types::ToolInvocationStarted::No
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("between 1 and 4 questions")
    );
}

#[test]
fn tool_actor_context_routes_ask_user_question_through_typed_handler() {
    struct AnswerHandler;

    impl RuntimeUserInputHandler for AnswerHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            assert_eq!(request.id, "ask-structured:question:1");
            assert_eq!(request.question, "Runtime: Which path?");
            assert_eq!(
                request.choices,
                vec![
                    "Reuse - Use the runtime broker".to_string(),
                    "New - Create another path".to_string()
                ]
            );
            Ok(Some("Reuse".to_string()))
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "ask-structured".to_string(),
        name: ToolName::AskUserQuestion,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "questions": [{
                    "header": "Runtime",
                    "question": "Which path?",
                    "options": [
                        {"label": "Reuse", "description": "Use the runtime broker"},
                        {"label": "New", "description": "Create another path"}
                    ]
                }]
            })
            .to_string(),
        ),
    };

    let result = context
        .execute_user_input_tool(&request, &AnswerHandler)
        .expect("structured user input result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.output.as_deref().unwrap()).unwrap(),
        serde_json::json!({"answers": {"Which path?": "Reuse"}})
    );
}

#[test]
fn tool_actor_context_grants_request_permissions_write_roots_for_current_turn() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": [extra.path()]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        context.granted_additional_working_directories(),
        vec![extra.path().to_path_buf()]
    );
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(
        output["granted"]["fileSystem"]["write"][0],
        extra.path().display().to_string()
    );
}

#[test]
fn tool_actor_context_grants_request_permissions_entry_write_roots() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": null,
                        "entries": [
                            {
                                "path": extra.path(),
                                "access": "write"
                            }
                        ]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        context.granted_additional_working_directories(),
        vec![extra.path().to_path_buf()]
    );
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(
        output["granted"]["fileSystem"]["write"][0],
        extra.path().display().to_string()
    );
}

#[test]
fn tool_actor_context_reports_request_permissions_network_domain_grants() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "grant-network".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Network,
        target: Some("api.example.com".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "reason": "fetch release metadata",
                "permissions": {
                    "fileSystem": null,
                    "network": {
                        "enabled": true,
                        "domains": {
                            "api.example.com": "allow"
                        }
                    }
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(output["granted"]["network"]["enabled"], true);
    assert_eq!(
        output["granted"]["network"]["domains"]["api.example.com"],
        "allow"
    );
}

#[test]
fn turn_permission_overlay_requests_and_merges_network_grants() {
    struct AllowNetwork;

    impl RuntimePermissionRequestHandler for AllowNetwork {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            assert_eq!(request.id, "net-tool");
            assert_eq!(
                request.reason.as_deref(),
                Some("tool attempted network access")
            );
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: true,
            })
        }
    }

    let mut overlay = TurnPermissionOverlay::default();
    let response = overlay
        .request_and_merge(
            &AllowNetwork,
            RuntimePermissionRequest {
                id: "net-tool".to_string(),
                reason: Some("tool attempted network access".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: Some(RequestNetworkPermissions {
                        enabled: None,
                        domains: std::collections::HashMap::from([(
                            "api.example.com".to_string(),
                            PermissionProfileNetworkAccess::Allow,
                        )]),
                    }),
                },
                context: RuntimePermissionContext::foreground(
                    orca_runtime::surface::SurfacePermissionOrigin::Unknown,
                ),
            },
        )
        .expect("permission request");

    assert_eq!(response.decision, PermissionResponseDecision::Allow);
    assert_eq!(
        overlay.network_domain_permissions().get("api.example.com"),
        Some(&PermissionProfileNetworkAccess::Allow)
    );
    assert!(overlay.strict_auto_review());
}

#[test]
fn turn_permission_overlay_requests_and_merges_file_system_write_grants() {
    struct AllowFileSystem {
        root: std::path::PathBuf,
    }

    impl RuntimePermissionRequestHandler for AllowFileSystem {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![self.root.clone()]),
                        entries: request
                            .permissions
                            .file_system
                            .as_ref()
                            .and_then(|file_system| file_system.entries.clone()),
                    }),
                    network: None,
                },
                strict_auto_review: false,
            })
        }
    }

    let root = tempfile::tempdir().expect("write root");
    let mut overlay = TurnPermissionOverlay::default();
    overlay
        .request_and_merge(
            &AllowFileSystem {
                root: root.path().to_path_buf(),
            },
            RuntimePermissionRequest {
                id: "fs-tool".to_string(),
                reason: Some("tool needs write access".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![root.path().to_path_buf()]),
                        entries: None,
                    }),
                    network: None,
                },
                context: RuntimePermissionContext::foreground(
                    orca_runtime::surface::SurfacePermissionOrigin::Unknown,
                ),
            },
        )
        .expect("permission request");

    assert_eq!(
        overlay.additional_working_directories(),
        &[root.path().to_path_buf()]
    );
}

#[test]
fn turn_permission_overlay_does_not_merge_denied_responses() {
    struct DenyNetwork;

    impl RuntimePermissionRequestHandler for DenyNetwork {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Deny,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: true,
            })
        }
    }

    let mut overlay = TurnPermissionOverlay::default();
    let response = overlay
        .request_and_merge(
            &DenyNetwork,
            RuntimePermissionRequest {
                id: "denied-network".to_string(),
                reason: Some("blocked network".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: Some(RequestNetworkPermissions {
                        enabled: None,
                        domains: std::collections::HashMap::from([(
                            "api.example.com".to_string(),
                            PermissionProfileNetworkAccess::Allow,
                        )]),
                    }),
                },
                context: RuntimePermissionContext::foreground(
                    orca_runtime::surface::SurfacePermissionOrigin::Unknown,
                ),
            },
        )
        .expect("permission request");

    assert_eq!(response.decision, PermissionResponseDecision::Deny);
    assert!(overlay.network_domain_permissions().is_empty());
    assert!(!overlay.strict_auto_review());
}

#[test]
fn tool_actor_context_includes_strict_auto_review_in_permission_output() {
    struct StrictHandler {
        root: std::path::PathBuf,
    }

    impl RuntimePermissionRequestHandler for StrictHandler {
        fn request_permissions(
            &self,
            _request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![self.root.clone()]),
                        entries: None,
                    }),
                    network: None,
                },
                strict_auto_review: true,
            })
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools");
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": [extra.path()]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool_with_handler(
        &request,
        &StrictHandler {
            root: extra.path().to_path_buf(),
        },
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(output["strictAutoReview"], true);
}

#[test]
fn task_actor_executes_normal_tool_with_runtime_policy() {
    let config = danger_full_access_config();
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let task_registry = TaskRegistry::new("run-actor".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf actor-tool".to_string()),
        raw_arguments: Some(serde_json::json!({ "command": "printf actor-tool" }).to_string()),
    };

    let result = actor.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        ToolConfig::default().shell_timeout_secs,
        Some(&task_registry),
        None,
        None,
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("actor-tool"));
    assert_eq!(result.exit_code, Some(0));
}

#[test]
fn tool_actor_context_allows_bash_writes_to_additional_working_directories() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("runtime-additional-roots-");
    let workspace = parent.path().join("workspace");
    let extra = parent.path().join("extra");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace dir");
    std::fs::create_dir(&extra).expect("extra dir");
    std::fs::create_dir(&outside).expect("outside dir");
    let extra_file = extra.join("allowed.txt");
    let outside_file = outside.join("blocked.txt");
    let mut context = RuntimeToolActorContext::new("run-tools");
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some(format!(
            "printf allowed > {} && printf blocked > {}",
            extra_file.display(),
            outside_file.display()
        )),
        raw_arguments: None,
    };

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &test_run_config(),
        &request,
        &workspace,
        std::slice::from_ref(&extra),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        5,
        Some(&task_registry),
        None,
        None,
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
    assert!(!outside_file.exists());
}

#[test]
fn tool_actor_context_retries_bash_after_filesystem_permission_grant() {
    if !sandbox_seatbelt_available() {
        return;
    }

    struct AllowRequestedFileSystem;

    impl RuntimePermissionRequestHandler for AllowRequestedFileSystem {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            let file_system = request
                .permissions
                .file_system
                .as_ref()
                .expect("filesystem permission request");
            let write_roots = file_system.write.as_ref().expect("write roots");
            assert_eq!(write_roots.len(), 1);

            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: false,
            })
        }
    }

    let parent = sandbox_test_parent("runtime-permission-grant-");
    let workspace = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace dir");
    std::fs::create_dir(&outside).expect("outside dir");
    let outside_file = outside.join("granted.txt");
    let mut context = RuntimeToolActorContext::new("run-tools");
    let mut config = test_run_config();
    config.cwd = Some(workspace.clone());
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some(format!("printf granted > {}", outside_file.display())),
        raw_arguments: None,
    };

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        &workspace,
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        5,
        Some(&task_registry),
        None,
        Some(&AllowRequestedFileSystem),
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "granted");
}

#[test]
fn tool_actor_context_retries_workspace_git_write_after_permission_grant() {
    if !sandbox_seatbelt_available() {
        return;
    }

    struct AllowGitDirectory {
        git_dir: std::path::PathBuf,
    }

    impl RuntimePermissionRequestHandler for AllowGitDirectory {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            let write_roots = request
                .permissions
                .file_system
                .as_ref()
                .and_then(|file_system| file_system.write.as_ref())
                .expect("filesystem write roots");
            assert_eq!(write_roots, &[self.git_dir.clone()]);

            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: false,
            })
        }
    }

    let repo = tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("repo");
    let git_dir = repo.path().join(".git");
    std::fs::create_dir(&git_dir).expect("git dir");
    let index_lock = git_dir.join("index.lock");
    let mut context = RuntimeToolActorContext::new("run-tools");
    let mut config = test_run_config();
    config.cwd = Some(repo.path().to_path_buf());
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some(format!("printf locked > {}", index_lock.display())),
        raw_arguments: None,
    };

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        repo.path(),
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        5,
        Some(&task_registry),
        None,
        Some(&AllowGitDirectory {
            git_dir: git_dir.clone(),
        }),
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(std::fs::read_to_string(index_lock).unwrap(), "locked");
}

#[test]
fn tool_actor_context_reports_git_index_lock_sandbox_denial() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent =
        tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("sandbox parent");
    let repo = parent.path().join("repo");
    let workspace = repo.join("web");
    let git_dir = repo.join(".git");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::create_dir(&git_dir).expect("git dir");
    let index_lock = git_dir.join("index.lock");
    let mut context = RuntimeToolActorContext::new("run-tools");
    let mut config = test_run_config();
    config.cwd = Some(workspace.clone());
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some(format!(
            "printf 'fatal: Unable to create '\\''{}'\\'': Operation not permitted\\n' >&2; exit 128",
            index_lock.display()
        )),
        raw_arguments: None,
    };

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        &workspace,
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        5,
        Some(&task_registry),
        None,
        None,
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    let output = result.error.as_deref().expect("tool error");
    assert!(output.contains("not a stale git lock"), "{output}");
    assert!(output.contains("sandbox"), "{output}");
    assert!(output.contains(&repo.display().to_string()), "{output}");
    assert!(
        output.contains(&workspace.display().to_string()),
        "{output}"
    );
}

#[test]
fn tool_actor_context_reuses_one_runtime_task_for_approval_hooks_and_execution() {
    let config = danger_full_access_config();
    let mut context = RuntimeToolActorContext::new("run-tools");
    let task_registry = orca_runtime::tasks::TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf actor-context".to_string()),
        raw_arguments: Some(serde_json::json!({ "command": "printf actor-context" }).to_string()),
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf actor-context".to_string()),
        preview: None,
    };
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreToolUse,
        command: "printf '%s' '{\"action\":\"allow\"}'".to_string(),
        tool: None,
    }]);

    let approval_decision = context.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::FullAuto),
        Some(approval),
        &request,
    );
    assert!(matches!(
        approval_decision,
        RuntimeApprovalDecision::Allowed(_)
    ));

    let pre_tool_outcome = context
        .run_pre_tool_hook(&hooks, &hook_test_cwd(), &request)
        .expect("pre tool hook");
    assert!(pre_tool_outcome.modified_target.is_none());
    assert!(pre_tool_outcome.injected_context.is_empty());

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        ToolConfig::default().shell_timeout_secs,
        Some(&task_registry),
        None,
        None,
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("actor-context"));
    let shell_tasks = task_registry.list();
    assert_eq!(shell_tasks.len(), 1);
    assert_eq!(shell_tasks[0].task_type, TaskType::Shell);
    assert_eq!(shell_tasks[0].status, TaskStatus::Completed);
    assert_eq!(
        shell_tasks[0].command.as_deref(),
        Some("printf actor-context")
    );

    assert!(
        context
            .run_post_tool_hook(
                &HookRunner::new(Vec::new()),
                &hook_test_cwd(),
                &request,
                &result
            )
            .is_none()
    );

    let task = context.active_task().expect("active task");
    assert_eq!(task.id(), "run-tools:task-1");
    assert_eq!(task.kind(), RuntimeTaskKind::Agent);
    assert_eq!(task.status(), RuntimeTaskStatus::Running);
}

#[test]
fn tool_actor_context_cancels_normal_tool_before_admission_without_shell_task() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let task_registry = orca_runtime::tasks::TaskRegistry::new("run-tools".to_string());
    let cancel = orca_core::cancel::CancelToken::new();
    cancel.cancel();
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf before; sleep 5; printf after".to_string()),
        raw_arguments: Some(
            serde_json::json!({ "command": "printf before; sleep 5; printf after" }).to_string(),
        ),
    };
    let start = std::time::Instant::now();

    let result = context.execute_normal_tool_with_cancel(
        &test_run_config(),
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        30,
        Some(&task_registry),
        Some(&cancel),
    );

    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancelled shell-session tool should not wait for the shell timeout"
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Cancelled);
    assert_eq!(
        result.kind,
        orca_core::tool_types::ToolResultKind::Cancelled
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("normal invocation was cancelled before dispatch"),
        "unexpected error: {:?}",
        result.error
    );
    assert!(
        task_registry
            .list()
            .iter()
            .all(|task| task.task_type != TaskType::Shell),
        "cancel-before-admission must not create a shell task record"
    );
    assert_eq!(
        result.terminal().started,
        orca_core::tool_types::ToolInvocationStarted::No
    );
}

#[test]
fn tool_actor_context_preserves_shell_session_timeout_as_failure() {
    let config = danger_full_access_config();
    let mut context = RuntimeToolActorContext::new("run-tools");
    let task_registry = orca_runtime::tasks::TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-timeout".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf before; sleep 5; printf after".to_string()),
        raw_arguments: Some(
            serde_json::json!({ "command": "printf before; sleep 5; printf after" }).to_string(),
        ),
    };
    let start = std::time::Instant::now();

    let result = context.execute_normal_tool_with_roots_and_cancel(
        &config,
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &[],
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        1,
        Some(&task_registry),
        None,
        None,
    );

    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "timed out shell-session tool should stop near its timeout"
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.kind,
        orca_core::tool_types::ToolResultKind::RuntimeError
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("shell command timed out after 1s"),
        "unexpected error: {:?}",
        result.error
    );
}

#[test]
fn tool_actor_context_task_stop_cancels_running_shell_task_wait() {
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let shell_registry = task_registry.clone();
    let handle = std::thread::spawn(move || {
        let config = danger_full_access_config();
        let mut context = RuntimeToolActorContext::new("run-tools");
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf before; sleep 30; printf after".to_string()),
            raw_arguments: Some(
                serde_json::json!({ "command": "printf before; sleep 30; printf after" })
                    .to_string(),
            ),
        };

        context.execute_normal_tool_with_roots_and_cancel(
            &config,
            &request,
            std::env::current_dir().expect("cwd").as_path(),
            &[],
            &McpRegistry::default(),
            &[],
            ToolConfig::default().output_truncation,
            30,
            Some(&shell_registry),
            None,
            None,
        )
    });
    let started = std::time::Instant::now();
    let task_id =
        loop {
            if let Some(task) = task_registry.list().into_iter().find(|task| {
                task.task_type == TaskType::Shell && task.status == TaskStatus::Running
            }) {
                break task.id;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "shell task did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
    let mut stop_context = RuntimeToolActorContext::new("run-tools");
    let stop_request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task_id)),
    };
    let stop_started = std::time::Instant::now();

    let stop_result = stop_context.execute_task_stop_tool(&stop_request, &task_registry);
    let result = handle.join().expect("shell thread result");

    assert_eq!(
        stop_result.status,
        orca_core::tool_types::ToolStatus::Completed
    );
    let stop_deadline = if cfg!(windows) {
        std::time::Duration::from_secs(10)
    } else {
        std::time::Duration::from_secs(2)
    };
    assert!(
        stop_started.elapsed() < stop_deadline,
        "task_stop should cancel the running shell wait promptly"
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Cancelled);
    assert_eq!(
        result.kind,
        orca_core::tool_types::ToolResultKind::Cancelled
    );
    assert!(
        task_registry
            .list()
            .iter()
            .any(|task| task.id == task_id && task.status == TaskStatus::Stopped),
        "task_stop should stop the shell task record"
    );
}

#[test]
fn tool_actor_context_classifies_runtime_special_tool_dispatch() {
    let context = RuntimeToolActorContext::new("run-tools");

    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowDraft), false),
        RuntimeSpecialToolDispatch::WorkflowDraft
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowDraftAction), false),
        RuntimeSpecialToolDispatch::WorkflowDraftAction
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Workflow), false),
        RuntimeSpecialToolDispatch::Workflow
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Subagent), false),
        RuntimeSpecialToolDispatch::Subagent
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::SubagentStatus), false),
        RuntimeSpecialToolDispatch::SubagentStatus
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::TaskList), false),
        RuntimeSpecialToolDispatch::TaskList
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::TaskStop), false),
        RuntimeSpecialToolDispatch::TaskStop
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::RequestPermissions), false),
        RuntimeSpecialToolDispatch::RequestPermissions
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::RequestUserInput), false),
        RuntimeSpecialToolDispatch::Normal
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::AskUserQuestion), false),
        RuntimeSpecialToolDispatch::RequestUserInput
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowReadMessages), false),
        RuntimeSpecialToolDispatch::WorkflowIpc
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Bash), false),
        RuntimeSpecialToolDispatch::Normal
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::GetGoal), true),
        RuntimeSpecialToolDispatch::GetGoal
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::CreateGoal), true),
        RuntimeSpecialToolDispatch::CreateGoal
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::UpdateGoal), true),
        RuntimeSpecialToolDispatch::UpdateGoal
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::UpdateGoal), false),
        RuntimeSpecialToolDispatch::Normal
    );
}

#[test]
fn tool_actor_context_executes_workflow_ipc_guardrail_without_child_context() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "mailbox".to_string(),
        name: ToolName::WorkflowReadMessages,
        action: ActionKind::Agent,
        target: Some("findings".to_string()),
        raw_arguments: Some(serde_json::json!({ "channel": "findings" }).to_string()),
    };

    let result = context.execute_workflow_ipc_tool(&request, None);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("only available inside workflow child agents")
    );
}

#[test]
fn tool_actor_context_executes_workflow_ipc_against_runtime_trait() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let ipc = FakeWorkflowIpc;
    let request = ToolRequest {
        id: "mailbox".to_string(),
        name: ToolName::WorkflowSendMessage,
        action: ActionKind::Agent,
        target: Some("findings".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "channel": "findings",
                "from": "worker-a",
                "message": { "status": "ready" }
            })
            .to_string(),
        ),
    };

    let result = context.execute_workflow_ipc_tool(&request, Some(&ipc));

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["channel"], "findings");
    assert_eq!(output["from"], "worker-a");
    assert_eq!(output["message"]["status"], "ready");
}

#[test]
fn tool_actor_context_executes_subagent_status_against_runtime_lookup() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "status".to_string(),
        name: ToolName::SubagentStatus,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(serde_json::json!({ "agent_id": "agent-1" }).to_string()),
    };
    let lookup = FakeSubagentStatusLookup;

    let result = context.execute_subagent_status_tool(&request, &lookup);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["agent_id"], "agent-1");
    assert_eq!(output["status"], "completed");
    assert_eq!(output["description"], "inspect auth");
    assert_eq!(output["agent_type"], "general");
    assert_eq!(output["output"], "finished async audit");
    assert_eq!(output["error"], Value::Null);
    assert_eq!(output["continuation_id"], "continuation-1");
    assert_eq!(output["attempt_id"], "attempt-2");
    assert_eq!(output["checkpoint_id"], "checkpoint-3");
    assert_eq!(output["resumable"], true);
    assert_eq!(output["indeterminate"], false);
}

#[test]
fn tool_actor_context_lists_tasks_with_package3_shape() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "tasks".to_string(),
        name: ToolName::TaskList,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some("{}".to_string()),
    };

    let result = context.execute_task_list_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["tasks"][0]["id"], task.id);
    assert_eq!(output["tasks"][0]["subject"], "Run server");
    assert_eq!(output["tasks"][0]["status"], "running");
    assert_eq!(output["tasks"][0]["task_type"], "shell");
    assert_eq!(output["tasks"][0]["command"], "npm run dev");
    assert_eq!(output["tasks"][0]["blockedBy"], serde_json::json!([]));
}

#[test]
fn tool_actor_context_stops_running_task_by_task_id() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["task_id"], task.id);
    assert_eq!(output["task_type"], "shell");
    assert_eq!(output["command"], "npm run dev");
    assert_eq!(output["message"], "Task stop requested");
    assert_eq!(
        registry.get(&task.id).expect("task record").status,
        TaskStatus::Stopping
    );
}

#[test]
fn tool_actor_context_stops_running_task_by_deprecated_shell_id_alias() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"shell_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["task_id"], task.id);
    assert_eq!(
        registry.get(&task.id).expect("task record").status,
        TaskStatus::Stopping
    );
}

#[test]
fn tool_actor_context_rejects_task_stop_without_id() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some("{}".to_string()),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("missing required field: task_id")
    );
}

#[test]
fn tool_actor_context_rejects_unknown_task_stop() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(r#"{"task_id":"missing-task"}"#.to_string()),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("task 'missing-task' not found")
    );
}

#[test]
fn tool_actor_context_rejects_terminal_task_stop() {
    let mut context = RuntimeToolActorContext::new("run-tools");
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.complete(&task.id, "done".to_string()).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("task is already completed and cannot be stopped")
    );
}

#[test]
fn tool_actor_context_executes_workflow_draft_preview() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut context = RuntimeToolActorContext::new("run-tools");
    let request = ToolRequest {
        id: "draft".to_string(),
        name: ToolName::WorkflowDraft,
        action: ActionKind::Write,
        target: Some("preview workflow".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "script": workflow_script()
            })
            .to_string(),
        ),
    };

    let result = context
        .execute_workflow_draft_tool(
            &request,
            RuntimeWorkflowDraftRequest {
                workflows_enabled: true,
                cwd: temp.path(),
                session_id: "session-1",
                max_concurrent_agents: 3,
            },
        )
        .expect("workflow draft result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["sessionId"], "session-1");
    assert_eq!(output["cwd"], temp.path().display().to_string());
    assert_eq!(output["name"], "runtime-draft");
    assert_eq!(output["description"], "Runtime draft");
    assert_eq!(output["phases"], serde_json::json!(["main"]));
    assert_eq!(output["estimatedAgentCount"], 1);
    assert_eq!(output["maxConfiguredConcurrentAgents"], 3);
    assert_eq!(output["sourceMutationRisk"], "read_only_likely");
    let script_path = std::path::Path::new(output["scriptPath"].as_str().expect("script path"));
    assert!(script_path.ends_with("script.js"));
    assert!(
        script_path.starts_with(
            temp.path()
                .join(".orca/workflow-sessions/session-1/workflow-drafts")
        )
    );
}

#[test]
fn controller_turn_started_events_include_agent_task_lifecycle() {
    let mut output = Vec::new();
    let mut config = test_run_config();
    config.provider = ProviderKind::Mock;
    config.output_format = OutputFormat::Jsonl;
    config.history_mode = HistoryMode::Disabled;
    config.approval_mode = ApprovalMode::FullAuto;
    config.prompt = "reply once".to_string();

    let exit = orca_runtime::controller::run_to_writer(config, &mut output);

    assert_eq!(exit, 0);
    let events = String::from_utf8(output)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("json event"))
        .collect::<Vec<_>>();
    let session_started = events
        .iter()
        .find(|event| event["type"] == "session.started")
        .expect("session.started event");
    let turn_started = events
        .iter()
        .find(|event| event["type"] == "turn.started")
        .expect("turn.started event");
    let session_completed = events
        .iter()
        .find(|event| event["type"] == "session.completed")
        .expect("session.completed event");

    assert_eq!(turn_started["payload"]["task"]["kind"], "agent");
    assert_eq!(turn_started["payload"]["task"]["status"], "running");
    assert_eq!(turn_started["payload"]["task"]["turn"], 1);
    assert_eq!(turn_started["runId"], session_started["runId"]);
    assert_eq!(session_completed["runId"], session_started["runId"]);
}

fn workflow_script() -> &'static str {
    "export const meta = { name: 'runtime-draft', description: 'Runtime draft', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
}

struct FakeSubagentStatusLookup;

impl RuntimeSubagentStatusLookup for FakeSubagentStatusLookup {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord> {
        if agent_id != "agent-1" {
            return None;
        }
        Some(RuntimeSubagentStatusRecord {
            id: agent_id.to_string(),
            status: "completed".to_string(),
            description: "inspect auth".to_string(),
            agent_type: Some("general".to_string()),
            created_at_ms: 1,
            started_at_ms: Some(2),
            completed_at_ms: Some(3),
            output: Some("finished async audit".to_string()),
            error: None,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation_id: Some("continuation-1".to_string()),
            continuation_attempt_id: Some("attempt-2".to_string()),
            continuation_checkpoint_id: Some("checkpoint-3".to_string()),
            continuation_resumable: true,
            continuation_indeterminate: false,
        })
    }
}

struct FakeWorkflowIpc;

impl RuntimeWorkflowIpc for FakeWorkflowIpc {
    fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({
            "channel": channel,
            "from": from.unwrap_or("default"),
            "message": message,
        }))
    }

    fn read_messages(&self, channel: &str) -> Result<Value, String> {
        Ok(serde_json::json!([{ "channel": channel }]))
    }

    fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        Ok(serde_json::json!({ "cleared": channel }))
    }

    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "items": items }))
    }

    fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "by": by }))
    }

    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({
            "name": name,
            "task_id": task_id,
            "result": result,
            "by": by,
        }))
    }

    fn list_tasks(&self, name: &str) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "tasks": [] }))
    }
}

fn tool_request(name: ToolName) -> ToolRequest {
    ToolRequest {
        id: "tool-1".to_string(),
        name,
        action: ActionKind::Read,
        target: None,
        raw_arguments: None,
    }
}

fn sandbox_seatbelt_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(
            orca_tools::sandbox::enforcement_state(),
            orca_core::capability::EnforcementState::Enforced
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn test_run_config() -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: Some(std::env::current_dir().expect("cwd")),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        model_runtime: Default::default(),
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
        permission_rules: PermissionRules::default(),
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
