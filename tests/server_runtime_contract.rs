use std::io::Write;

use orca_core::approval_rules::{PermissionRule, PermissionRules};
use orca_core::approval_types::{ApprovalMode, Decision};
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::event_schema::EventType;
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::thread_identity::TurnId;
use orca_runtime::controller::{
    ControllerRunOptions, ThreadTurnContext, ThreadTurnExecution, ThreadTurnExecutor,
    ThreadTurnRequest, run_thread_turn_to_writer,
};
use orca_runtime::history::{
    SessionStore, SortDirection, ThreadListFilters, ThreadSortKey, ThreadStore,
};
use orca_runtime::lifecycle::ThreadSteerHandle;
use orca_runtime::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind};
use orca_runtime::server::{
    ActivePermissionProfile, AdditionalWorkingDirectory, JsonlSurfaceAdapter,
    PermissionProfileOverride, PermissionRuleValue, PermissionUpdate, ServerThreadTurn,
};
use orca_runtime::session::InteractiveSession;
use serde_json::Value;
use tempfile::tempdir;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn server_thread_owns_live_session_projection_and_turns() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut runtime = start_server_runtime();
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let projection = runtime
            .read_thread(&thread_id, false, false)
            .expect("read thread");

        assert_eq!(projection.thread_id, thread_id);
        assert_eq!(projection.title, "(empty prompt)");
        assert_eq!(projection.cwd, home.display().to_string());
        assert_eq!(projection.message_count, 1);
        assert!(projection.messages.is_empty());
        assert!(projection.turns.is_empty());

        let mut first_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                "first prompt",
                request_writer(&mut first_output),
            )
            .expect("run first turn");
        assert!(
            parse_jsonl(&first_output)
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );

        let mut second_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                "mock_history_echo",
                request_writer(&mut second_output),
            )
            .expect("run second turn");
        let echoed = parse_jsonl(&second_output)
            .iter()
            .filter(|event| event["event"] == "message_delta")
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(
            echoed.contains("first prompt | mock_history_echo"),
            "expected second turn to see prior thread history, got: {echoed}"
        );

        let projection = runtime
            .read_thread(&thread_id, true, true)
            .expect("read completed thread");
        assert_eq!(projection.message_count, 5);
        assert!(
            projection
                .messages
                .iter()
                .any(|item| item["role"] == "user" && item["content"] == "first prompt")
        );
    });
}

#[test]
fn server_thread_runs_explicit_turn_requests() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut runtime = start_server_runtime();
        let thread_id = runtime.start_thread(&config).expect("start thread");

        let turn = ServerThreadTurn::new("explicit turn request");
        assert_eq!(turn.prompt(), "explicit turn request");

        let mut output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                turn.prompt(),
                request_writer(&mut output),
            )
            .expect("run explicit turn request");

        let events = parse_jsonl(&output);
        assert!(
            events.iter().any(|event| event["event"] == "turn_started"),
            "turn request should stream turn_started"
        );
        let projection = runtime
            .read_thread(&thread_id, true, false)
            .expect("read completed thread");
        assert!(
            projection
                .messages
                .iter()
                .any(|item| item["role"] == "user" && item["content"] == "explicit turn request")
        );
    });
}

#[test]
fn server_thread_runtime_resolves_completed_turn_owner() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut runtime = start_server_runtime();
        let thread_id = runtime.start_thread(&config).expect("start thread");

        runtime
            .run_turn(&config, &thread_id, "completed turn owner", Vec::new())
            .expect("run turn");

        let turns = runtime
            .list_thread_turns(
                &thread_id,
                None,
                1,
                orca_runtime::history::SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("turns");
        let turn_id = turns.data[0].turn_id.clone();

        assert_eq!(
            runtime.completed_turn_thread_id(&turn_id).as_deref(),
            Some(thread_id.as_str())
        );
        assert_eq!(runtime.completed_turn_thread_id("turn-missing"), None);
    });
}

#[test]
fn server_thread_runtime_emits_opaque_unique_turn_ids() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut runtime = start_server_runtime();
        let first_thread_id = runtime.start_thread(&config).expect("start first thread");
        let second_thread_id = runtime.start_thread(&config).expect("start second thread");

        let mut first_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &first_thread_id,
                "first logical turn",
                request_writer(&mut first_output),
            )
            .expect("run first thread");
        let mut second_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &second_thread_id,
                "second logical turn",
                request_writer(&mut second_output),
            )
            .expect("run second thread");

        let turn_id = |output: &[u8]| {
            parse_jsonl(output)
                .into_iter()
                .find(|event| event["event"] == "turn_started")
                .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
                .expect("turn started identity")
        };
        let first_turn_id = turn_id(&first_output);
        let second_turn_id = turn_id(&second_output);

        assert!(first_turn_id.starts_with("turn_"));
        assert!(second_turn_id.starts_with("turn_"));
        assert_ne!(first_turn_id, second_turn_id);
    });
}

#[test]
fn recorded_server_turn_journals_the_emitted_turn_id() {
    with_orca_home(|home| {
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let mut runtime = start_server_runtime();
        let thread_id = runtime
            .start_thread(&config)
            .expect("start recorded thread");
        let mut output = Vec::new();

        runtime
            .run_turn(
                &config,
                &thread_id,
                "record logical identity",
                request_writer(&mut output),
            )
            .expect("run recorded turn");

        let emitted_turn_id = parse_jsonl(&output)
            .into_iter()
            .find(|event| event["event"] == "turn_started")
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("emitted turn identity");
        TurnId::parse(emitted_turn_id.clone()).expect("typed emitted turn identity");

        let transcript = SessionStore::new()
            .load_session(&thread_id)
            .expect("load recorded transcript");
        let journaled = transcript
            .semantic_events
            .iter()
            .find(|event| event.event_type == EventType::TurnStarted)
            .expect("journaled turn start");

        assert_eq!(journaled.payload["turn_id"], emitted_turn_id);
    });
}

#[test]
fn recorded_server_turn_persists_identified_conversation_records() {
    with_orca_home(|home| {
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let mut runtime = start_server_runtime();
        let thread_id = runtime
            .start_thread(&config)
            .expect("start recorded thread");
        let mut output = Vec::new();

        runtime
            .run_turn(
                &config,
                &thread_id,
                "persist stable item identity",
                request_writer(&mut output),
            )
            .expect("run recorded turn");

        let emitted_turn_id = parse_jsonl(&output)
            .into_iter()
            .find(|event| event["event"] == "turn_started")
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("emitted turn identity");
        let transcript = SessionStore::new()
            .load_session(&thread_id)
            .expect("load recorded transcript");
        let records = std::fs::read_to_string(&transcript.path)
            .expect("read recorded conversation")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| {
                record["type"] == "conversation.message" && record["message"]["role"] != "system"
            })
            .collect::<Vec<_>>();

        assert!(!records.is_empty(), "expected persisted turn messages");
        for record in records {
            assert!(
                record["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("item_")),
                "new conversation records must own opaque item ids: {record}"
            );
            assert_eq!(record["turn_id"], emitted_turn_id);
        }
    });
}

#[test]
fn thread_turn_executor_runs_interactive_session_turns() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "", None).expect("session");
        let mut lifecycle = RuntimeSessionLifecycle::new("thread-turn-executor");
        lifecycle.start_task(RuntimeTaskKind::Agent);
        let mut output = Vec::new();

        let mut executor = ThreadTurnExecutor::new(&config, &mut session, &mut lifecycle);
        executor
            .run_request(
                &ThreadTurnRequest::new("executor turn").with_wait_for_background_workflows(false),
                request_writer(&mut output),
            )
            .expect("run executor turn");

        assert!(
            parse_jsonl(&output)
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );
        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. }
                        if content == "executor turn"
                ))
        );
    });
}

#[test]
fn thread_turn_executor_runs_explicit_turn_requests() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "", None).expect("session");
        let mut lifecycle = RuntimeSessionLifecycle::new("thread-turn-request");
        lifecycle.start_task(RuntimeTaskKind::Agent);
        let request = ThreadTurnRequest::new("executor request turn")
            .with_wait_for_background_workflows(false);
        assert_eq!(request.prompt(), "executor request turn");
        assert!(!request.options().wait_for_background_workflows);

        let mut output = Vec::new();
        ThreadTurnExecutor::new(&config, &mut session, &mut lifecycle)
            .run_request(&request, request_writer(&mut output))
            .expect("run executor request");

        assert!(
            parse_jsonl(&output)
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );
        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. }
                        if content == "executor request turn"
                ))
        );
    });
}

#[test]
fn thread_turn_executor_injects_pending_steer_input_before_model_call() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "", None).expect("session");
        let mut lifecycle = RuntimeSessionLifecycle::new("thread-turn-steer");
        lifecycle.start_task(RuntimeTaskKind::Agent);
        let steer = ThreadSteerHandle::default();
        steer.push("mock_history_echo");
        let request = ThreadTurnRequest::new("initial active turn")
            .with_wait_for_background_workflows(false)
            .with_steer_handle(steer);

        let mut output = Vec::new();
        ThreadTurnExecutor::new(&config, &mut session, &mut lifecycle)
            .run_request(&request, request_writer(&mut output))
            .expect("run steered request");

        let message_text = parse_jsonl(&output)
            .iter()
            .filter(|event| event["event"] == "message_delta")
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(
            message_text.contains("initial active turn | mock_history_echo"),
            "steer input should be visible to the provider, got: {message_text}"
        );
        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. }
                        if content == "mock_history_echo"
                ))
        );
    });
}

#[test]
fn thread_turn_legacy_writer_uses_same_request_boundary() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "", None).expect("session");
        let mut lifecycle = RuntimeSessionLifecycle::new("thread-turn-legacy");
        lifecycle.start_task(RuntimeTaskKind::Agent);
        let mut output = Vec::new();

        run_thread_turn_to_writer(
            &config,
            &mut session,
            &mut lifecycle,
            "legacy request turn",
            request_writer(&mut output),
            ControllerRunOptions {
                wait_for_background_workflows: false,
            },
        )
        .expect("run legacy writer turn");

        assert!(
            parse_jsonl(&output)
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );
        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. }
                        if content == "legacy request turn"
                ))
        );
    });
}

#[test]
fn thread_turn_context_prepares_session_prompt() {
    with_orca_home(|home| {
        let config = test_run_config(home);
        let mut session =
            InteractiveSession::new_with_preloaded(&config, "", None).expect("session");
        let request = ThreadTurnRequest::new("context prompt");

        {
            let context =
                ThreadTurnContext::prepare(&config, &mut session, &request).expect("context");
            assert_eq!(context.prompt(), "context prompt");
            assert_eq!(
                context.cwd().display().to_string(),
                home.display().to_string()
            );
        }

        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. }
                        if content == "context prompt"
                ))
        );
    });
}

#[test]
fn thread_turn_execution_owns_runtime_event_state() {
    let mut lifecycle = RuntimeSessionLifecycle::new("thread-turn-execution");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let output = Vec::new();

    let execution = ThreadTurnExecution::new(&lifecycle, output, OutputFormat::Jsonl);

    assert_eq!(execution.run_id(), "thread-turn-execution");
    assert_eq!(execution.background_workflow_count(), 0);
}

#[test]
fn server_thread_runtime_starts_thread_with_live_projection() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;

        let thread_id = runtime.start_thread(&config).expect("start thread");
        let projection = runtime
            .read_thread(&thread_id, false, false)
            .expect("read live thread");

        assert_eq!(projection.thread_id, thread_id);
        assert_eq!(projection.title, "(empty prompt)");
        assert_eq!(projection.cwd, home.display().to_string());
        assert_eq!(projection.message_count, 1);
        assert!(projection.messages.is_empty());
        assert!(projection.turns.is_empty());
    });
}

#[test]
fn server_thread_runtime_materializes_started_thread_in_session_store() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;

        let thread_id = runtime.start_thread(&config).expect("start thread");

        let stored = SessionStore::new()
            .read_thread(&thread_id, false, false)
            .expect("read stored thread");
        assert_eq!(stored.thread_id, thread_id);
        assert_eq!(stored.title, "(empty prompt)");
        assert_eq!(stored.cwd, home.display().to_string());
        assert_eq!(stored.message_count, 1);
    });
}

#[test]
fn recorded_thread_live_and_cold_projections_keep_ids_across_resume() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let mut first_output = Vec::new();

        runtime
            .run_turn(
                &config,
                &thread_id,
                "first stable turn",
                request_writer(&mut first_output),
            )
            .expect("run first turn");
        let first_turn_id = parse_jsonl(&first_output)
            .into_iter()
            .find(|event| event["event"] == "turn_started")
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("first emitted turn id");

        let live_turns = runtime
            .list_thread_turns(
                &thread_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("live turns");
        let live_items = runtime
            .list_thread_items(&thread_id, None, None, usize::MAX, SortDirection::Asc)
            .expect("live items");
        let store = SessionStore::new();
        let cold_turns = store
            .list_thread_turns(
                &thread_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("cold turns");
        let cold_items = store
            .list_thread_items(&thread_id, None, None, usize::MAX, SortDirection::Asc)
            .expect("cold items");

        assert_eq!(live_turns, cold_turns);
        assert_eq!(live_items, cold_items);
        assert_eq!(cold_turns.data[0].turn_id, first_turn_id);
        assert!(
            cold_items
                .data
                .iter()
                .all(|item| item.item_id.starts_with("item_"))
        );
        let first_item_ids = cold_items
            .data
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>();

        runtime
            .resume_thread(&config, &thread_id)
            .expect("resume recorded thread");
        let mut second_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                "second stable turn",
                request_writer(&mut second_output),
            )
            .expect("run resumed turn");
        let second_turn_id = parse_jsonl(&second_output)
            .into_iter()
            .find(|event| event["event"] == "turn_started")
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("second emitted turn id");
        let resumed_turns = store
            .list_thread_turns(
                &thread_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("resumed turns");
        let resumed_items = store
            .list_thread_items(&thread_id, None, None, usize::MAX, SortDirection::Asc)
            .expect("resumed items");

        assert_eq!(resumed_turns.data[0].turn_id, first_turn_id);
        assert_eq!(resumed_turns.data[1].turn_id, second_turn_id);
        assert_eq!(
            &resumed_items
                .data
                .iter()
                .take(first_item_ids.len())
                .map(|item| item.item_id.clone())
                .collect::<Vec<_>>(),
            &first_item_ids
        );

        let expected_turn_ids = resumed_turns
            .data
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        let expected_item_ids = resumed_items
            .data
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>();
        drop(runtime);
        store
            .rename_session(&thread_id, "stable identity history")
            .expect("rename recorded thread");
        store
            .archive_session(&thread_id)
            .expect("archive recorded thread");
        store
            .compress_session(&thread_id)
            .expect("compress archived thread");

        let rewritten_turns = store
            .list_thread_turns(
                &thread_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("rewritten turns");
        let rewritten_items = store
            .list_thread_items(&thread_id, None, None, usize::MAX, SortDirection::Asc)
            .expect("rewritten items");
        assert_eq!(
            rewritten_turns
                .data
                .iter()
                .map(|turn| turn.turn_id.clone())
                .collect::<Vec<_>>(),
            expected_turn_ids
        );
        assert_eq!(
            rewritten_items
                .data
                .iter()
                .map(|item| item.item_id.clone())
                .collect::<Vec<_>>(),
            expected_item_ids
        );
    });
}

#[test]
fn server_thread_runtime_resumes_and_forks_persisted_threads() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;

        let parent_id = runtime.start_thread(&config).expect("start thread");
        runtime
            .run_turn(&config, &parent_id, "first prompt", Vec::new())
            .expect("run parent turn");

        let resumed_id = runtime
            .resume_thread(&config, &parent_id)
            .expect("resume thread");
        assert_eq!(resumed_id, parent_id);

        let mut resumed_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &resumed_id,
                "mock_history_echo",
                request_writer(&mut resumed_output),
            )
            .expect("run resumed turn");
        let echoed = parse_jsonl(&resumed_output)
            .iter()
            .filter(|event| event["event"] == "message_delta")
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(
            echoed.contains("first prompt | mock_history_echo"),
            "expected resumed thread to see persisted history, got: {echoed}"
        );
        let resumed_projection = runtime
            .read_thread(&parent_id, true, true)
            .expect("read resumed same-id thread");
        assert_eq!(resumed_projection.thread_id, parent_id);
        assert!(resumed_projection.messages.iter().any(|message| {
            message["role"] == "user" && message["content"] == "mock_history_echo"
        }));

        let forked_id = runtime
            .fork_thread(&config, &parent_id)
            .expect("fork thread");
        assert_ne!(forked_id, parent_id);

        let stored_fork = SessionStore::new()
            .read_thread(&forked_id, false, false)
            .expect("read stored fork");
        assert_eq!(stored_fork.thread_id, forked_id);
        let inherited_turns = SessionStore::new()
            .list_thread_turns(
                &forked_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("read inherited fork turns");
        let inherited_turn_ids = inherited_turns
            .data
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        assert!(!inherited_turn_ids.is_empty());
        assert!(
            inherited_turn_ids
                .iter()
                .all(|turn_id| turn_id.starts_with("turn-"))
        );

        let mut fork_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &forked_id,
                "new fork turn",
                request_writer(&mut fork_output),
            )
            .expect("run fork turn");
        let emitted_fork_turn_id = parse_jsonl(&fork_output)
            .into_iter()
            .find(|event| event["event"] == "turn_started")
            .and_then(|event| event["turnId"].as_str().map(ToString::to_string))
            .expect("emitted fork turn id");
        let fork_turns = SessionStore::new()
            .list_thread_turns(
                &forked_id,
                None,
                usize::MAX,
                SortDirection::Asc,
                orca_runtime::history::TurnItemsView::Full,
            )
            .expect("read fork turns after execution");
        assert_eq!(
            fork_turns
                .data
                .iter()
                .take(inherited_turn_ids.len())
                .map(|turn| turn.turn_id.clone())
                .collect::<Vec<_>>(),
            inherited_turn_ids
        );
        assert_eq!(
            fork_turns.data.last().map(|turn| turn.turn_id.as_str()),
            Some(emitted_fork_turn_id.as_str())
        );
    });
}

#[test]
fn server_thread_runtime_resume_and_fork_inherit_stored_permission_profile() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut original = test_run_config(home);
        original.history_mode = HistoryMode::Record;
        original.active_permission_profile = Some(ActivePermissionProfile::new(
            "locked-down",
            Some(":workspace"),
        ));
        original.approval_mode = ApprovalMode::Plan;
        original.permission_rules = PermissionRules {
            rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
        };

        let parent_id = runtime.start_thread(&original).expect("start thread");

        let mut current = test_run_config(home);
        current.history_mode = HistoryMode::Record;
        current.approval_mode = ApprovalMode::FullAuto;
        current.permission_rules = PermissionRules::default();

        let resumed_id = runtime
            .resume_thread(&current, &parent_id)
            .expect("resume thread");
        let forked_id = runtime
            .fork_thread(&current, &parent_id)
            .expect("fork thread");

        let store = SessionStore::new();
        let summaries = store
            .list_threads(
                None,
                10,
                ThreadListFilters::default(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads")
            .data;
        let resumed = summaries
            .iter()
            .find(|thread| thread.thread_id == resumed_id)
            .expect("resumed summary");
        assert_eq!(resumed.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(
            resumed.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(resumed.permission_rule_count, 1);

        let forked = summaries
            .iter()
            .find(|thread| thread.thread_id == forked_id)
            .expect("forked summary");
        assert_eq!(forked.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(
            forked.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(forked.permission_rule_count, 1);
    });
}

#[test]
fn server_thread_runtime_resume_and_fork_apply_explicit_permission_override() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut original = test_run_config(home);
        original.history_mode = HistoryMode::Record;
        original.active_permission_profile = Some(ActivePermissionProfile::new(
            "locked-down",
            Some(":workspace"),
        ));
        original.approval_mode = ApprovalMode::Plan;
        original.permission_rules = PermissionRules {
            rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
        };

        let parent_id = runtime.start_thread(&original).expect("start thread");

        let mut current = test_run_config(home);
        current.history_mode = HistoryMode::Record;
        current.approval_mode = ApprovalMode::FullAuto;
        current.permission_rules = PermissionRules {
            rules: vec![PermissionRule::new("bash", "npm *", Decision::Deny)],
        };
        let override_profile = PermissionProfileOverride {
            active_permission_profile: Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace"),
            )),
            approval_mode: Some(ApprovalMode::AutoEdit),
            runtime_workspace_roots: None,
            permission_rules: Some(PermissionRules {
                rules: vec![PermissionRule::new(
                    "bash",
                    "cargo test *",
                    Decision::Prompt,
                )],
            }),
            permission_updates: Vec::new(),
        };

        let resumed_id = runtime
            .resume_thread_with_permissions(&current, &parent_id, override_profile.clone())
            .expect("resume thread");
        let forked_id = runtime
            .fork_thread_with_permissions(&current, &parent_id, override_profile)
            .expect("fork thread");

        let store = SessionStore::new();
        let summaries = store
            .list_threads(
                None,
                10,
                ThreadListFilters::default(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads")
            .data;
        for thread_id in [&resumed_id, &forked_id] {
            let thread = summaries
                .iter()
                .find(|thread| thread.thread_id == *thread_id)
                .expect("listed thread");
            assert_eq!(thread.approval_mode, Some(ApprovalMode::AutoEdit));
            assert_eq!(
                thread.active_permission_profile,
                Some(ActivePermissionProfile::new(
                    "workspace-plus",
                    Some(":workspace")
                ))
            );
            assert_eq!(thread.permission_rule_count, 1);
        }

        let resumed = store.load_session(&resumed_id).expect("load resumed");
        assert_eq!(
            resumed.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(resumed.meta.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            resumed.meta.permission_rules.rules[0].pattern,
            "cargo test *"
        );
        assert_eq!(
            resumed.meta.permission_rules.rules[0].decision,
            Decision::Prompt
        );

        let forked = store.load_session(&forked_id).expect("load forked");
        assert_eq!(
            forked.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(forked.meta.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            forked.meta.permission_rules.rules[0].pattern,
            "cargo test *"
        );
        assert_eq!(
            forked.meta.permission_rules.rules[0].decision,
            Decision::Prompt
        );
    });
}

#[test]
fn server_thread_runtime_turn_start_applies_persistent_permission_override() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        config.approval_mode = ApprovalMode::Plan;
        config.permission_rules = PermissionRules {
            rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
        };
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let override_profile = PermissionProfileOverride {
            active_permission_profile: None,
            approval_mode: Some(ApprovalMode::FullAuto),
            runtime_workspace_roots: None,
            permission_rules: Some(PermissionRules {
                rules: vec![PermissionRule::new(
                    "bash",
                    "cargo test *",
                    Decision::Prompt,
                )],
            }),
            permission_updates: Vec::new(),
        };

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                override_profile,
                Vec::new(),
            )
            .expect("run turn with permissions");

        let store = SessionStore::new();
        let persisted = store.load_session(&thread_id).expect("load session");
        assert_eq!(persisted.meta.approval_mode, Some(ApprovalMode::FullAuto));
        assert_eq!(persisted.meta.permission_rules.rules.len(), 1);
        assert_eq!(
            persisted.meta.permission_rules.rules[0].pattern,
            "cargo test *"
        );

        let summaries = store
            .list_threads(
                None,
                10,
                ThreadListFilters::default(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads")
            .data;
        let thread = summaries
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .expect("listed thread");
        assert_eq!(thread.approval_mode, Some(ApprovalMode::FullAuto));
        assert_eq!(thread.permission_rule_count, 1);
    });
}

#[test]
fn server_thread_runtime_turn_start_applies_incremental_permission_updates() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        config.approval_mode = ApprovalMode::Plan;
        config.permission_rules = PermissionRules {
            rules: vec![
                PermissionRule::new("bash", "cargo *", Decision::Allow),
                PermissionRule::new("bash", "rm -rf *", Decision::Deny),
                PermissionRule::new("write_file", "/tmp/**", Decision::Prompt),
            ],
        };
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let override_profile = PermissionProfileOverride {
            active_permission_profile: None,
            approval_mode: None,
            runtime_workspace_roots: None,
            permission_rules: None,
            permission_updates: vec![
                PermissionUpdate::SetMode {
                    destination: "session".to_string(),
                    mode: ApprovalMode::FullAuto,
                },
                PermissionUpdate::RemoveRules {
                    destination: "session".to_string(),
                    behavior: Decision::Allow,
                    rules: vec![PermissionRuleValue::new("bash", Some("cargo *"))],
                },
                PermissionUpdate::AddRules {
                    destination: "session".to_string(),
                    behavior: Decision::Allow,
                    rules: vec![PermissionRuleValue::new("bash", Some("cargo test *"))],
                },
                PermissionUpdate::ReplaceRules {
                    destination: "session".to_string(),
                    behavior: Decision::Prompt,
                    rules: vec![PermissionRuleValue::new(
                        "write_file",
                        Some("/workspace/**"),
                    )],
                },
            ],
        };

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                override_profile,
                Vec::new(),
            )
            .expect("run turn with permission updates");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");
        assert_eq!(persisted.meta.approval_mode, Some(ApprovalMode::FullAuto));
        assert_eq!(
            persisted.meta.permission_rules.rules,
            vec![
                PermissionRule::new("bash", "rm -rf *", Decision::Deny),
                PermissionRule::new("bash", "cargo test *", Decision::Allow),
                PermissionRule::new("write_file", "/workspace/**", Decision::Prompt),
            ]
        );
    });
}

#[test]
fn server_thread_runtime_turn_start_persists_directory_permission_updates() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let extra = home.join("extra");
        let removed = home.join("removed");
        std::fs::create_dir_all(&extra).expect("extra dir");
        std::fs::create_dir_all(&removed).expect("removed dir");
        config.additional_working_directories =
            vec![AdditionalWorkingDirectory::new(removed.clone(), "session")];
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let override_profile = PermissionProfileOverride {
            active_permission_profile: None,
            approval_mode: None,
            runtime_workspace_roots: None,
            permission_rules: None,
            permission_updates: vec![
                PermissionUpdate::AddDirectories {
                    directories: vec![AdditionalWorkingDirectory::new(extra.clone(), "session")],
                },
                PermissionUpdate::RemoveDirectories {
                    destination: "session".to_string(),
                    directories: vec![removed.clone()],
                },
            ],
        };

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                override_profile,
                Vec::new(),
            )
            .expect("run turn with directory updates");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");
        assert_eq!(
            persisted.meta.additional_working_directories,
            vec![AdditionalWorkingDirectory::new(extra, "session")]
        );
    });
}

#[test]
fn server_thread_directory_update_cannot_forge_metadata_escalation() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let git_dir = home.join(".git");
        std::fs::create_dir_all(&git_dir).expect("git dir");
        let thread_id = runtime.start_thread(&config).expect("start thread");
        let override_profile = PermissionProfileOverride {
            active_permission_profile: None,
            approval_mode: None,
            runtime_workspace_roots: None,
            permission_rules: None,
            permission_updates: vec![PermissionUpdate::AddDirectories {
                directories: vec![AdditionalWorkingDirectory::new(
                    git_dir.clone(),
                    "session-metadata",
                )],
            }],
        };

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                override_profile,
                Vec::new(),
            )
            .expect("run turn with forged directory source");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");
        assert!(
            !persisted
                .meta
                .additional_working_directories
                .iter()
                .any(|directory| {
                    directory.path == git_dir && directory.source == "session-metadata"
                }),
            "reserved metadata sources must be ignored by ordinary directory updates"
        );
        assert!(
            persisted.meta.metadata_writable_directories.is_empty(),
            "ordinary permission updates must not mint metadata escalation authority"
        );
    });
}

#[test]
fn server_thread_start_cannot_forge_metadata_escalation() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let git_dir = home.join(".git");
        std::fs::create_dir_all(&git_dir).expect("git dir");
        config.additional_working_directories = vec![AdditionalWorkingDirectory::new(
            git_dir.clone(),
            "session-metadata",
        )];

        let thread_id = runtime.start_thread(&config).expect("start thread");
        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");

        assert!(
            persisted.meta.additional_working_directories.is_empty(),
            "reserved metadata sources must be ignored at the runtime boundary"
        );
        assert!(
            persisted.meta.metadata_writable_directories.is_empty(),
            "runtime configuration must not mint metadata escalation authority"
        );
    });
}

#[test]
fn server_thread_runtime_turn_start_persists_runtime_workspace_roots() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let old_root = home.join("old-root");
        let new_root = home.join("new-root");
        std::fs::create_dir_all(&old_root).expect("old root");
        std::fs::create_dir_all(&new_root).expect("new root");
        config.runtime_workspace_roots = Some(vec![old_root.clone()]);
        let thread_id = runtime.start_thread(&config).expect("start thread");

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                PermissionProfileOverride {
                    active_permission_profile: None,
                    approval_mode: None,
                    runtime_workspace_roots: Some(vec![new_root.clone()]),
                    permission_rules: None,
                    permission_updates: Vec::new(),
                },
                Vec::new(),
            )
            .expect("run turn with runtime workspace roots");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");
        assert_eq!(
            persisted.meta.runtime_workspace_roots,
            vec![new_root.clone()]
        );
        let projection = runtime
            .read_thread(&thread_id, false, false)
            .expect("read live projection");
        assert_eq!(projection.runtime_workspace_roots, vec![new_root]);
    });
}

#[test]
fn server_thread_runtime_turn_start_persists_active_permission_profile() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let mut config = test_run_config(home);
        config.history_mode = HistoryMode::Record;
        let thread_id = runtime.start_thread(&config).expect("start thread");

        runtime
            .run_turn_with_permissions(
                &config,
                &thread_id,
                "mock_history_echo",
                PermissionProfileOverride {
                    active_permission_profile: Some(ActivePermissionProfile::new(
                        "locked-down",
                        Some(":workspace"),
                    )),
                    approval_mode: None,
                    runtime_workspace_roots: None,
                    permission_rules: None,
                    permission_updates: Vec::new(),
                },
                Vec::new(),
            )
            .expect("run turn with active profile");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load session");
        assert_eq!(
            persisted.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
    });
}

#[test]
fn server_thread_runtime_runs_turn_and_preserves_conversation() {
    with_orca_home(|home| {
        let mut runtime = start_server_runtime();
        let config = test_run_config(home);
        let thread_id = runtime.start_thread(&config).expect("start thread");

        let mut first_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                "first prompt",
                request_writer(&mut first_output),
            )
            .expect("run first turn");
        let first_events = parse_jsonl(&first_output);
        assert!(
            first_events
                .iter()
                .any(|event| event["event"] == "turn_started")
        );
        assert!(
            first_events
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );

        let mut second_output = Vec::new();
        runtime
            .run_turn(
                &config,
                &thread_id,
                "mock_history_echo",
                request_writer(&mut second_output),
            )
            .expect("run second turn");
        let second_events = parse_jsonl(&second_output);
        let echoed = second_events
            .iter()
            .filter(|event| event["event"] == "message_delta")
            .filter_map(|event| event["text"].as_str())
            .collect::<String>();
        assert!(
            echoed.contains("first prompt | mock_history_echo"),
            "expected second turn to see prior thread history, got: {echoed}"
        );

        let projection = runtime
            .read_thread(&thread_id, true, true)
            .expect("read live projection");
        assert_eq!(projection.message_count, 5);
        assert!(
            projection
                .messages
                .iter()
                .any(|item| { item["role"] == "user" && item["content"] == "first prompt" })
        );
        assert!(projection.turns.iter().any(|turn| {
            turn.thread_id == thread_id
                && turn
                    .items
                    .iter()
                    .any(|item| item["role"] == "user" && item["content"] == "mock_history_echo")
        }));
    });
}

fn start_server_runtime() -> JsonlSurfaceAdapter {
    JsonlSurfaceAdapter::start().expect("start server runtime")
}

fn request_writer<'a>(output: &'a mut Vec<u8>) -> impl Write + 'a {
    orca_runtime::server::ServerRequestWriter::new(Value::from("turn"), output)
}

fn test_run_config(cwd: &std::path::Path) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).expect("model"),
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

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("temp home");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = f(home.path());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    result
}
