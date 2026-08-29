use super::*;
use crate::protocol::{
    PendingWorkflowNotification, TuiEvent, TuiInteractionKey, TuiInteractionKind, UserAction,
};
use crate::surface_projection::SurfaceProjectionState;
use crate::transcript_state::ChatMessage;
use crate::viewport_state::CopyNotice;
use orca_core::plan_types::PlanStatus;
use orca_core::task_types::{TaskStatus, TaskType};

fn inserted_source_line<'a>(
    lines: &'a [ratatui::text::Line<'static>],
    source: &str,
) -> &'a ratatui::text::Line<'static> {
    lines
        .iter()
        .find(|line| {
            line.to_string().contains(source)
                && line
                    .spans
                    .first()
                    .is_some_and(|span| span.content.ends_with("+ "))
        })
        .unwrap_or_else(|| panic!("inserted source line containing {source:?}"))
}

fn normalized_source_spans(
    spans: &[ratatui::text::Span<'_>],
) -> crate::syntax_highlight::StyledSourceLine {
    let mut output: crate::syntax_highlight::StyledSourceLine = Vec::new();
    for span in spans {
        let mut style = span.style;
        style.bg = None;
        if let Some(previous) = output.last_mut()
            && previous.style == style
        {
            previous.content.to_mut().push_str(span.content.as_ref());
            continue;
        }
        output.push(ratatui::text::Span::styled(span.content.to_string(), style));
    }
    output
}

fn state() -> AppState {
    let (tx, _rx) = mpsc::unbounded();
    AppState::new(
        tx,
        "0.0.0-test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    )
}

fn queued(text: &str) -> crate::queued_input::QueuedUserMessage {
    crate::queued_input::QueuedUserMessage::from_composer(
        text.to_string(),
        Vec::new(),
        orca_runtime::mentions::MentionBindings::default(),
    )
    .unwrap()
}

#[test]
#[ignore = "runtime queue projection replaces local lifecycle reset"]
fn conversation_replacement_resets_all_queued_follow_up_state() {
    for clear in [false, true] {
        let mut state = state();
        state.enqueue_user_message(queued("in flight")).unwrap();
        state.set_status(AppStatus::Idle);
        state
            .begin_next_queued_message()
            .expect("seed in-flight queued submission");
        state.enqueue_user_message(queued("queued")).unwrap();
        state.suspend_queued_follow_up_autosend();
        state.report_queued_input_error("full".to_string());

        if clear {
            state.clear_messages();
        } else {
            state.replace_messages([ChatMessage::System("replacement".to_string())]);
        }

        assert!(state.queued_pending_visible_text().is_empty());
        assert!(!state.queued_submission_in_flight());
        assert!(state.queued_autosend_enabled());
        assert!(state.queued_input_error().is_none());
        assert!(!state.queued_follow_up_pending_or_in_flight());

        state.enqueue_user_message(queued("after reset")).unwrap();
        state.set_status(AppStatus::Idle);
        state.begin_next_queued_message().unwrap();
        assert_eq!(state.queued_in_flight_id(), Some(1));
    }
}

#[test]
fn approval_closes_search_but_preserves_query() {
    let mut state = state();
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval"),
        tool: "bash".to_string(),
        target: None,
        preview: None,
    });

    assert!(!state.transcript.search.open);
    assert_eq!(state.transcript.search.query(), "target");
}

#[test]
fn fresh_app_state_has_default_syntax_highlight_state() {
    let state = state();

    assert!(state.workspace_git.is_none());
    assert!(state.syntax_workspace_root_for_test().is_none());
    assert_eq!(
        state.syntax_theme_for_test(),
        crate::syntax_highlight::SyntaxTheme::OneHalfDark
    );
    assert_eq!(
        state.syntax_color_level_for_test(),
        crate::terminal_capabilities::TerminalColorLevel::TrueColor
    );
    assert!(!state.edit_highlight_runtime_started_for_test());
    assert!(state.edit_highlights.applied().is_empty());
}

fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
    TuiInteractionKey::new(
        orca_core::cancel::OperationIdAllocator::new().allocate(),
        id,
        kind,
    )
}

fn dummy_selection() -> crate::selection::TranscriptSelection {
    let pos = crate::selection::SelectionPos { row: 0, col: 0 };
    let end = crate::selection::SelectionPos { row: 1, col: 3 };
    crate::selection::TranscriptSelection {
        anchor: pos,
        head: end,
        dragging: false,
        granularity: crate::selection::SelectionGranularity::Cell,
        origin: (pos, end),
    }
}

#[test]
fn transcript_mutations_invalidate_the_selection_only_when_rows_can_shift() {
    let mut state = state();
    state.push_message(ChatMessage::System("one".to_string()));
    state.push_message(ChatMessage::System("two".to_string()));
    state.push_message(ChatMessage::System("three".to_string()));

    // Appending and rewriting the TAIL keep the selection: earlier rows
    // cannot move.
    state.viewport.selection = Some(dummy_selection());
    state.push_message(ChatMessage::System("four".to_string()));
    assert!(state.viewport.selection.is_some());
    state.touch_message(state.transcript.messages.len() - 1);
    assert!(state.viewport.selection.is_some());

    // Rewriting a non-tail message can change its height: cleared.
    state.touch_message(1);
    assert_eq!(state.viewport.selection, None);

    // Removing messages shifts rows: cleared.
    state.viewport.selection = Some(dummy_selection());
    state.truncate_messages(3);
    assert_eq!(state.viewport.selection, None);

    state.viewport.selection = Some(dummy_selection());
    state.retain_messages(|message| !matches!(message, ChatMessage::System(text) if text == "two"));
    assert_eq!(state.viewport.selection, None);

    // A retain that keeps everything moves nothing: selection survives.
    state.viewport.selection = Some(dummy_selection());
    state.retain_messages(|_| true);
    assert!(state.viewport.selection.is_some());

    state.viewport.selection = Some(dummy_selection());
    state.clear_messages();
    assert_eq!(state.viewport.selection, None);
}

#[test]
fn history_loaded_replaces_legacy_prefix_and_freezes_snapshot() {
    let mut state = state();
    state.push_message(ChatMessage::User("legacy".to_string()));

    state.update(TuiEvent::HistoryLoaded {
        messages: vec![
            ChatMessage::User("restored".to_string()),
            ChatMessage::Assistant("answer".to_string()),
        ],
        plan: Some((
            Some("resume plan".to_string()),
            vec![PlanItem {
                step: "continue".to_string(),
                status: PlanStatus::InProgress,
            }],
        )),
        label: "Resumed saved conversation.".to_string(),
    });

    assert!(matches!(
        state.transcript.messages.as_slice(),
        [
            ChatMessage::User(prompt),
            ChatMessage::Assistant(answer),
            ChatMessage::System(label),
        ] if prompt == "restored"
            && answer == "answer"
            && label == "Resumed saved conversation."
    ));
    assert_eq!(
        state.transcript.finalized_count,
        state.transcript.messages.len()
    );
    // `flushed_count` must stay 0 in the fullscreen TUI: it counts messages
    // omitted from the live renderer, so setting it to the message count made
    // `live_start` skip the whole transcript and blanked the pane on switch.
    assert_eq!(state.transcript.flushed_count, 0);
    assert_eq!(
        state.current_plan().unwrap().0.as_deref(),
        Some("resume plan")
    );
    assert_eq!(state.status, AppStatus::Idle);
}

#[test]
fn new_session_started_resets_conversation_state_and_preserves_runtime_settings() {
    let mut state = state();
    state.push_message(ChatMessage::User("old prompt".to_string()));
    state.replace_plan_for_test(Some((
        Some("old plan".to_string()),
        vec![PlanItem {
            step: "old step".to_string(),
            status: PlanStatus::InProgress,
        }],
    )));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("old-session".to_string()),
            title: "Old session".to_string(),
            usage_revision: 1,
            usage: UsageTotals {
                input_tokens: 42,
                ..UsageTotals::default()
            },
            context_revision: 1,
            context_used_tokens: 21,
            context_limit_tokens: 100,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        },
    )));
    state.approval_allowlist.insert("bash".to_string());
    state.model_name = "deepseek-v4-pro".to_string();
    state.reasoning_effort = orca_core::config::ReasoningEffort::High;
    state.approval_mode = ApprovalMode::FullAuto;
    state.history_cursor = Some(0);
    state.draft_before_history = Some("old draft".to_string());
    state.last_ctrl_c = Some(Instant::now());
    state.viewport.pending_clipboard_copy = Some("old selection".to_string());
    state.viewport.copy_notice = Some(CopyNotice {
        chars: 13,
        at: Instant::now(),
        local_only: false,
    });
    state.viewport.last_left_click = Some((Instant::now(), 1, 1, 1));
    state.viewport.composer_mouse_selecting = true;
    state.enter_running();

    state.update(TuiEvent::SessionProjectionReset(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(2),
            session_id: Some("019f8a00-0000-7000-8000-000000000123".to_string()),
            title: "New conversation".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        },
    )));
    state.update(TuiEvent::NewSessionStarted);

    assert!(state.transcript.messages.is_empty());
    assert!(state.current_plan().is_none());
    assert_eq!(state.usage(), &UsageTotals::default());
    assert_eq!(state.context_used_tokens(), 0);
    assert_eq!(state.context_limit_tokens(), 0);
    assert!(state.approval_allowlist.is_empty());
    assert_eq!(state.status, AppStatus::Idle);
    assert_eq!(state.model_name, "deepseek-v4-pro");
    assert_eq!(
        state.reasoning_effort,
        orca_core::config::ReasoningEffort::High
    );
    assert_eq!(state.approval_mode, ApprovalMode::FullAuto);
    assert!(state.history_cursor.is_none());
    assert!(state.draft_before_history.is_none());
    assert!(state.last_ctrl_c.is_none());
    assert!(state.viewport.pending_clipboard_copy.is_none());
    assert!(state.viewport.copy_notice.is_none());
    assert!(state.viewport.last_left_click.is_none());
    assert!(!state.viewport.composer_mouse_selecting);
}

#[test]
fn surface_session_projection_updates_current_identity() {
    let mut state = state();

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("session-1".to_string()),
            title: "Auth investigation".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        },
    )));

    assert_eq!(state.current_session_id(), Some("session-1"));
    assert_eq!(state.current_session_title(), Some("Auth investigation"));
}

fn session(id: &str, title: &str) -> SessionSummary {
    use chrono::Utc;
    SessionSummary {
        session_id: id.to_string(),
        title: title.to_string(),
        cwd: "/tmp".to_string(),
        provider: "deepseek".to_string(),
        model: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        path: std::env::temp_dir(),
        archived: false,
        parent_id: None,
        forked: false,
        approval_mode: None,
        active_permission_profile: None,
        runtime_workspace_roots: Vec::new(),
        permission_rule_count: 0,
        additional_working_directories: Vec::new(),
        network_domain_permissions: Default::default(),
    }
}

fn workflow_task_summary(id: &str, name: &str) -> BackgroundTaskSummary {
    BackgroundTaskSummary {
        id: id.to_string(),
        task_type: TaskType::Workflow,
        status: TaskStatus::Running,
        is_backgrounded: false,
        description: name.to_string(),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: None,
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        pending_tool_call: None,
        name: Some(name.to_string()),
        workflow_run_id: Some(format!("run-{id}")),
        phase_count: Some(1),
        workflow_progress: None,
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        workflow_script_path: None,
        workflow_launch_input: None,
        workflow_final_summary: None,
        workflow_failure_count: 0,
        usage: None,
        subagent_current_activity: None,
        subagent_turn: None,
        last_activity_at_ms: None,
        continuation: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
        publication_revision: None,
    }
}

#[test]
fn workflow_notification_action_carries_notification_boundary() {
    let expected = PendingWorkflowNotification {
        id: "notice-1".to_string(),
        prompt: "continue the workflow".to_string(),
    };
    let action = UserAction::SubmitWorkflowNotification(expected.clone());

    match action {
        UserAction::SubmitWorkflowNotification(actual) => {
            assert_eq!(actual, expected);
        }
        _ => unreachable!("constructed the workflow notification variant"),
    }
}

#[test]
fn session_search_filters_by_title_and_keeps_selection_valid() {
    let mut state = state();
    state.session_picker_sessions = vec![
        session("a", "fix the failing auth test"),
        session("b", "add JWT auth middleware"),
        session("c", "refactor parser entrypoint"),
    ];
    state.session_picker_selected = 0;

    // No query → all match.
    assert_eq!(state.filtered_session_indices(), vec![0, 1, 2]);

    // Typing "auth" keeps only the two auth sessions and snaps selection
    // to the first match.
    for ch in "auth".chars() {
        state.session_query_push(ch);
    }
    assert_eq!(state.filtered_session_indices(), vec![0, 1]);
    assert_eq!(state.session_picker_selected, 0);

    // Down moves within the filtered set, not the raw list.
    state.select_next_session();
    assert_eq!(state.session_picker_selected, 1);
    state.select_next_session();
    assert_eq!(state.session_picker_selected, 1); // clamped to last match

    // Backspace widens the filter again.
    state.session_query_pop();
    assert_eq!(state.session_picker_query, "aut");
    assert_eq!(state.filtered_session_indices(), vec![0, 1]);
}

#[test]
fn approval_options_have_numeric_primary_keys_and_legacy_shortcuts() {
    assert_eq!(ApprovalOption::Once.key(), '1');
    assert_eq!(ApprovalOption::AlwaysTarget.key(), '2');
    assert_eq!(ApprovalOption::AlwaysTool.key(), '3');
    assert_eq!(ApprovalOption::Deny.key(), '4');

    assert!(ApprovalOption::Once.matches_key('1'));
    assert!(ApprovalOption::Once.matches_key('y'));
    assert!(ApprovalOption::AlwaysTarget.matches_key('2'));
    assert!(ApprovalOption::AlwaysTarget.matches_key('A'));
    assert!(ApprovalOption::AlwaysTool.matches_key('3'));
    assert!(ApprovalOption::AlwaysTool.matches_key('a'));
    assert!(ApprovalOption::Deny.matches_key('4'));
    assert!(ApprovalOption::Deny.matches_key('n'));

    assert!(!ApprovalOption::AlwaysTarget.matches_key('a'));
    assert!(!ApprovalOption::AlwaysTool.matches_key('A'));
}

#[test]
fn approval_dialog_resolves_numeric_and_legacy_keys_by_visible_options() {
    let dialog = ApprovalDialog {
        id: "approval-1".to_string(),
        interaction: None,
        tool: "edit".to_string(),
        target: Some("src/main.rs".to_string()),
        permission_kind: None,
        background_task_id: None,
        selected: 0,
        options: ApprovalDialog::options_for("edit", Some("src/main.rs")),
        diff: None,
    };

    assert_eq!(dialog.option_for_key('1'), Some(ApprovalOption::Once));
    assert_eq!(
        dialog.option_for_key('2'),
        Some(ApprovalOption::AlwaysTarget)
    );
    assert_eq!(dialog.option_for_key('3'), Some(ApprovalOption::AlwaysTool));
    assert_eq!(dialog.option_for_key('4'), Some(ApprovalOption::Deny));
    assert_eq!(dialog.option_for_key('y'), Some(ApprovalOption::Once));
    assert_eq!(
        dialog.option_for_key('A'),
        Some(ApprovalOption::AlwaysTarget)
    );
    assert_eq!(dialog.option_for_key('a'), Some(ApprovalOption::AlwaysTool));
    assert_eq!(dialog.option_for_key('n'), Some(ApprovalOption::Deny));

    let dynamic = ApprovalDialog {
        id: "approval-2".to_string(),
        interaction: None,
        tool: "web_search".to_string(),
        target: Some("query".to_string()),
        permission_kind: None,
        background_task_id: None,
        selected: 0,
        options: ApprovalDialog::options_for("web_search", Some("query")),
        diff: None,
    };
    assert_eq!(dynamic.option_for_key('2'), None);
    assert_eq!(
        dynamic.option_for_key('3'),
        Some(ApprovalOption::AlwaysTool)
    );
}

#[test]
fn approval_dialog_has_four_options_with_target_and_three_without() {
    // Static-target tool (like read_file) shows AlwaysTarget option.
    let with_target = ApprovalDialog::options_for("read_file", Some("src/auth/token.rs"));
    assert_eq!(
        with_target,
        vec![
            ApprovalOption::Once,
            ApprovalOption::AlwaysTarget,
            ApprovalOption::AlwaysTool,
            ApprovalOption::Deny,
        ]
    );
    // No target — AlwaysTarget is hidden.
    let without = ApprovalDialog::options_for("read_file", None);
    assert_eq!(
        without,
        vec![
            ApprovalOption::Once,
            ApprovalOption::AlwaysTool,
            ApprovalOption::Deny,
        ]
    );
    // Dynamic-target tool (web_search) — AlwaysTarget is hidden even with a target.
    let dynamic = ApprovalDialog::options_for("web_search", Some("some query"));
    assert_eq!(
        dynamic,
        vec![
            ApprovalOption::Once,
            ApprovalOption::AlwaysTool,
            ApprovalOption::Deny,
        ]
    );
}

#[test]
fn approval_allowlist_grants_matching_tool_and_target() {
    let mut tool_scope = state();

    // Initially nothing is allow-listed.
    assert!(!tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));

    // "Always allow tool" grants every target for that tool.
    tool_scope
        .approval_allowlist
        .insert(AppState::approval_key_tool("edit"));
    assert!(tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));
    assert!(tool_scope.approval_is_allowlisted("edit", Some("src/b.rs")));
    assert!(!tool_scope.approval_is_allowlisted("bash", Some("ls")));

    // "Always allow tool + target" is scoped to that one target.
    let mut scoped = state();
    scoped
        .approval_allowlist
        .insert(AppState::approval_key_target("bash", "cargo test"));
    assert!(scoped.approval_is_allowlisted("bash", Some("cargo test")));
    assert!(!scoped.approval_is_allowlisted("bash", Some("rm -rf /")));
}

#[test]
fn approval_needed_event_populates_dialog_options_and_diff() {
    let mut state = state();
    state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
        tool: "edit".to_string(),
        target: Some("src/auth/token.rs".to_string()),
        preview: Some("@@ token.rs @@\n- a\n+ b".to_string()),
    });
    let dialog = state.approval_dialog.expect("dialog present");
    assert_eq!(dialog.id, "approval-1");
    assert_eq!(dialog.options.len(), 4);
    assert!(dialog.diff.is_some());
    assert_eq!(dialog.current(), ApprovalOption::Once);
}

#[test]
fn subagent_events_update_existing_message() {
    let mut state = state();

    state.update(TuiEvent::SubagentStarted {
        id: "agent-1".to_string(),
        description: "inspect repo".to_string(),
    });
    state.update(TuiEvent::SubagentCompleted {
        id: "agent-1".to_string(),
        description: "inspect repo".to_string(),
        status: "completed".to_string(),
        output: Some("done".to_string()),
        error: None,
    });

    assert_eq!(state.transcript.messages.len(), 1);
    match &state.transcript.messages[0] {
        ChatMessage::Subagent {
            id,
            description,
            status,
            output,
            error,
            ..
        } => {
            assert_eq!(id, "agent-1");
            assert_eq!(description, "inspect repo");
            assert_eq!(status, "completed");
            assert_eq!(output.as_deref(), Some("done"));
            assert!(error.is_none());
        }
        other => panic!("expected subagent message, got {other:?}"),
    }
}

#[test]
fn subagent_progress_updates_existing_message_without_adding_rows() {
    let mut state = state();

    state.update(TuiEvent::SubagentStarted {
        id: "agent-1".to_string(),
        description: "inspect repo".to_string(),
    });
    state.update(TuiEvent::SubagentProgress {
        id: "agent-1".to_string(),
        activity: "bash: echo child".to_string(),
        turn: Some(1),
        usage: None,
    });

    assert_eq!(state.transcript.messages.len(), 1);
    match &state.transcript.messages[0] {
        ChatMessage::Subagent {
            id,
            status,
            activity,
            activity_tail,
            turn,
            ..
        } => {
            assert_eq!(id, "agent-1");
            assert_eq!(status, "running");
            assert_eq!(activity.as_deref(), Some("bash: echo child"));
            assert_eq!(activity_tail, &vec!["bash: echo child".to_string()]);
            assert_eq!(*turn, Some(1));
        }
        other => panic!("expected subagent message, got {other:?}"),
    }
}

#[test]
fn subagent_progress_retains_recent_activity_tail() {
    let mut state = state();

    state.update(TuiEvent::SubagentStarted {
        id: "agent-1".to_string(),
        description: "inspect repo".to_string(),
    });
    for index in 1..=8 {
        state.update(TuiEvent::SubagentProgress {
            id: "agent-1".to_string(),
            activity: format!("activity {index}"),
            turn: Some(index),
            usage: None,
        });
    }

    match &state.transcript.messages[0] {
        ChatMessage::Subagent {
            activity_tail,
            turn,
            ..
        } => {
            assert_eq!(*turn, Some(8));
            assert_eq!(activity_tail.len(), 6);
            assert_eq!(
                activity_tail.first().map(String::as_str),
                Some("activity 3")
            );
            assert_eq!(activity_tail.last().map(String::as_str), Some("activity 8"));
        }
        other => panic!("expected subagent message, got {other:?}"),
    }
}

#[test]
fn expand_toggle_flips_latest_live_subagent() {
    let mut state = state();

    state.update(TuiEvent::SubagentStarted {
        id: "agent-1".to_string(),
        description: "inspect repo".to_string(),
    });

    assert!(state.toggle_latest_tool_output());
    match &state.transcript.messages[0] {
        ChatMessage::Subagent { expanded, .. } => assert!(*expanded),
        other => panic!("expected subagent message, got {other:?}"),
    }
}

#[test]
fn completed_subagent_without_start_adds_message() {
    let mut state = state();

    state.update(TuiEvent::SubagentCompleted {
        id: "agent-2".to_string(),
        description: "review code".to_string(),
        status: "failed".to_string(),
        output: None,
        error: Some("boom".to_string()),
    });

    assert_eq!(state.transcript.messages.len(), 1);
    match &state.transcript.messages[0] {
        ChatMessage::Subagent {
            id,
            description,
            status,
            output,
            error,
            ..
        } => {
            assert_eq!(id, "agent-2");
            assert_eq!(description, "review code");
            assert_eq!(status, "failed");
            assert!(output.is_none());
            assert_eq!(error.as_deref(), Some("boom"));
        }
        other => panic!("expected subagent message, got {other:?}"),
    }
}

#[test]
fn generic_subagent_tool_events_do_not_create_tool_rows() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "tool-subagent".to_string(),
        name: "subagent".to_string(),
        target: Some("inspect repo".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "tool-subagent".to_string(),
        name: "subagent".to_string(),
        status: "completed".to_string(),
        output: "Subagent status: success".to_string(),
        diff: None,
        kind: Some("success".to_string()),
    });

    assert!(state.transcript.messages.is_empty());
}

#[test]
fn plan_lives_in_panel_during_turn_and_archives_inline_on_completion() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "tool-plan".to_string(),
        name: "update_plan".to_string(),
        target: Some("2 items".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "tool-plan".to_string(),
        name: "update_plan".to_string(),
        status: "completed".to_string(),
        output: "Plan updated".to_string(),
        diff: None,
        kind: Some("success".to_string()),
    });
    state.update(TuiEvent::PlanUpdated {
        explanation: Some("starting".to_string()),
        plan: vec![
            PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                step: "Patch".to_string(),
                status: PlanStatus::InProgress,
            },
        ],
    });

    // During the turn the plan only lives in the bottom panel, not the scrollback.
    assert!(state.transcript.messages.is_empty());
    assert!(state.current_plan().is_some());

    // When the turn completes the panel clears and the plan is archived inline.
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert!(state.current_plan().is_none());
    assert_eq!(state.transcript.messages.len(), 1);
    match &state.transcript.messages[0] {
        ChatMessage::PlanUpdate { explanation, plan } => {
            assert_eq!(explanation.as_deref(), Some("starting"));
            assert_eq!(plan.len(), 2);
            assert_eq!(plan[1].step, "Patch");
        }
        other => panic!("expected plan update message, got {other:?}"),
    }
}

#[test]
fn proposed_plan_tags_stream_as_dedicated_tui_message() {
    let mut state = state();

    state.update(TuiEvent::MessageDelta("Intro\n<proposed".to_string()));
    state.update(TuiEvent::MessageDelta(
        "_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro".to_string(),
    ));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    assert_eq!(state.transcript.messages.len(), 3);
    match &state.transcript.messages[0] {
        ChatMessage::Assistant(text) => assert_eq!(text, "Intro\n"),
        other => panic!("expected assistant preface, got {other:?}"),
    }
    match &state.transcript.messages[1] {
        ChatMessage::ProposedPlan(text) => assert_eq!(text, "# Plan\n- inspect\n"),
        other => panic!("expected proposed plan, got {other:?}"),
    }
    match &state.transcript.messages[2] {
        ChatMessage::Assistant(text) => assert_eq!(text, "\nOutro"),
        other => panic!("expected assistant postscript, got {other:?}"),
    }
}

#[test]
fn failed_plan_update_marks_panel_stale_until_next_success() {
    let mut state = state();

    state.update(TuiEvent::PlanUpdated {
        explanation: None,
        plan: vec![PlanItem {
            step: "Inspect".to_string(),
            status: PlanStatus::InProgress,
        }],
    });
    assert!(!state.plan_update_failed());

    state.update(TuiEvent::ToolCompleted {
        id: "tool-plan-2".to_string(),
        name: "update_plan".to_string(),
        status: "failed".to_string(),
        output: "tool arguments failed schema validation".to_string(),
        diff: None,
        kind: Some("error".to_string()),
    });
    assert!(
        state.plan_update_failed(),
        "failed update must mark the panel stale"
    );
    assert!(
        state.current_plan().is_some(),
        "the stale plan stays visible"
    );

    state.update(TuiEvent::PlanUpdated {
        explanation: None,
        plan: vec![PlanItem {
            step: "Inspect".to_string(),
            status: PlanStatus::Completed,
        }],
    });
    assert!(
        !state.plan_update_failed(),
        "a successful update clears the stale marker"
    );
}

#[test]
fn turn_completion_clears_plan_stale_marker() {
    let mut state = state();
    state.update(TuiEvent::PlanUpdated {
        explanation: None,
        plan: vec![PlanItem {
            step: "Inspect".to_string(),
            status: PlanStatus::Pending,
        }],
    });
    state.update(TuiEvent::ToolCompleted {
        id: "tool-plan".to_string(),
        name: "update_plan".to_string(),
        status: "failed".to_string(),
        output: "schema validation".to_string(),
        diff: None,
        kind: Some("error".to_string()),
    });
    assert!(state.plan_update_failed());

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert!(!state.plan_update_failed());
}

#[test]
fn session_completion_finalizes_the_turn_and_freezes_it() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state.update(TuiEvent::MessageDelta("answer".to_string()));

    // Mid-turn nothing is finalized: the whole transcript is still live.
    assert_eq!(state.transcript.finalized_count, 0);

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    // After completion every message is frozen.
    assert_eq!(
        state.transcript.finalized_count,
        state.transcript.messages.len()
    );
    assert!(state.transcript.finalized_count > 0);
    assert!(
        state.plan_approval_dialog.is_none(),
        "update_plan is a task checklist, not a proposed plan"
    );
}

#[test]
fn successful_plan_turn_opens_approval_only_for_current_proposed_plan() {
    let mut state = state();
    state.approval_mode = ApprovalMode::Plan;
    state.pre_plan_approval_mode = Some(ApprovalMode::AutoEdit);
    state.enter_running();
    state.update(TuiEvent::MessageDelta(
        "<proposed_plan>\n# Plan\n1. Inspect\n2. Implement\n</proposed_plan>".to_string(),
    ));

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    let dialog = state
        .plan_approval_dialog
        .as_ref()
        .expect("completed proposed plan should request approval");
    assert!(dialog.plan.contains("# Plan"));
    assert_eq!(dialog.selected, 0);
    assert_eq!(state.status, AppStatus::Idle);
    assert!(!state.queued_autosend_enabled());

    state.plan_approval_dialog = None;
    state.resume_queued_follow_up_autosend();
    state.enter_running();
    state.update(TuiEvent::MessageDelta(
        "A clarification is still needed.".to_string(),
    ));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert!(
        state.plan_approval_dialog.is_none(),
        "a historical plan must not reopen approval"
    );
}

#[test]
fn proposed_plan_outside_plan_mode_or_failed_turn_does_not_open_approval() {
    for (mode, status) in [
        (ApprovalMode::AutoEdit, "success"),
        (ApprovalMode::Plan, "failed"),
    ] {
        let mut state = state();
        state.approval_mode = mode;
        state.enter_running();
        state.update(TuiEvent::MessageDelta(
            "<proposed_plan>\n- inspect\n</proposed_plan>".to_string(),
        ));
        state.update(TuiEvent::SessionCompleted {
            status: status.to_string(),
        });
        assert!(state.plan_approval_dialog.is_none(), "{mode:?} {status}");
    }
}

#[test]
fn settings_transition_remembers_and_restores_pre_plan_mode() {
    let mut state = state();
    state.approval_mode = ApprovalMode::FullAuto;

    state.update(TuiEvent::SettingsUpdated {
        model: "model".to_string(),
        reasoning_effort: orca_core::config::ReasoningEffort::High,
        approval_mode: ApprovalMode::Plan,
    });
    assert_eq!(state.pre_plan_approval_mode, Some(ApprovalMode::FullAuto));

    state.update(TuiEvent::SettingsUpdated {
        model: "model".to_string(),
        reasoning_effort: orca_core::config::ReasoningEffort::High,
        approval_mode: ApprovalMode::FullAuto,
    });
    assert_eq!(state.pre_plan_approval_mode, None);
}

#[test]
fn expand_toggle_only_affects_live_tools_not_flushed_ones() {
    let mut state = state();

    // Turn 1: a tool call that gets completed.
    state.update(TuiEvent::ToolRequested {
        id: "t1".to_string(),
        name: "grep".to_string(),
        target: Some("a".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "t1".to_string(),
        name: "grep".to_string(),
        status: "completed".to_string(),
        output: "hit".to_string(),
        diff: None,
        kind: Some("success".to_string()),
    });
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    // Simulate the render loop flushing the settled prefix into scrollback: once
    // `flushed_count` covers the tool it is committed to the immutable scrollback.
    state.transcript.flushed_count = state.transcript.messages.len();

    // The flushed tool is frozen: `e` finds nothing in the (empty) live pane.
    assert!(!state.toggle_latest_tool_output());
    let ChatMessage::ToolCall { expanded, .. } = &state.transcript.messages[0] else {
        panic!("expected flushed tool call");
    };
    assert!(!expanded, "flushed tool must stay collapsed");

    // Turn 2: a new live tool call (beyond `flushed_count`) can be expanded.
    state.update(TuiEvent::ToolRequested {
        id: "t2".to_string(),
        name: "grep".to_string(),
        target: Some("b".to_string()),
    });
    assert!(state.toggle_latest_tool_output());
    let ChatMessage::ToolCall { expanded, .. } = state.transcript.messages.last().unwrap() else {
        panic!("expected live tool call");
    };
    assert!(expanded, "live tool should toggle expanded");
}

#[test]
fn clearing_messages_resets_the_finalized_watermark() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert!(state.transcript.finalized_count > 0);

    state.transcript.messages.clear();
    state.transcript.finalized_count = 0;

    // Watermark must never dangle past the (now empty) message list.
    assert_eq!(state.transcript.finalized_count, 0);
    assert!(state.transcript.messages.is_empty());
}

#[test]
fn backtrack_clamps_watermark_into_remaining_messages() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("first".to_string()));
    state
        .transcript
        .messages
        .push(ChatMessage::Assistant("reply".to_string()));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    let finalized_before = state.transcript.finalized_count;
    assert_eq!(finalized_before, 2);

    // A second user prompt starts a new live turn, then we backtrack it away.
    state
        .transcript
        .messages
        .push(ChatMessage::User("second".to_string()));
    state.remove_after_last_user();

    // Everything from the last user prompt onward is gone, and the watermark is
    // clamped so it can never exceed the remaining message count.
    assert!(state.transcript.finalized_count <= state.transcript.messages.len());
    assert_eq!(state.transcript.messages.len(), 2);
}

#[test]
fn submission_rejection_removes_optimistic_user_and_returns_idle() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant("before".to_string()));
    state.push_message(ChatMessage::User("review @gone.txt".to_string()));
    state.enter_running();
    state.update(TuiEvent::ToolCallProgress {
        id: "receiving".to_string(),
        name: Some("read_file".to_string()),
        arguments_bytes: 128,
    });

    state.update(TuiEvent::SubmissionRejected {
        queued_id: None,
        prompt: "review @gone.txt".to_string(),
        bindings: MentionBindings::default(),
        images: Vec::new(),
        message: "bound file is no longer available".to_string(),
    });

    assert_eq!(state.status, AppStatus::Idle);
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [ChatMessage::Assistant(before), ChatMessage::Error(error)]
            if before == "before" && error == "bound file is no longer available"
    ));
    assert!(state.mention_bindings.is_empty());
    assert_eq!(state.running_started_at, None);
    assert!(state.transcript.messages.iter().all(|message| {
        !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
    }));
}

#[test]
fn generic_error_does_not_end_a_running_turn() {
    let mut state = state();
    state.enter_running();

    state.update(TuiEvent::Error("recoverable runtime error".to_string()));

    assert_eq!(state.status, AppStatus::Running);
    assert!(state.running_started_at.is_some());
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Error(message)) if message == "recoverable runtime error"
    ));
}

#[test]
fn operation_rejection_reports_error_and_returns_idle() {
    let mut state = state();
    state.enter_running();

    state.update(TuiEvent::OperationRejected(
        "operation could not start".to_string(),
    ));

    assert_eq!(state.status, AppStatus::Idle);
    assert_eq!(state.running_started_at, None);
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Error(message)) if message == "operation could not start"
    ));
}

#[test]
fn recovery_projection_is_not_overwritten_by_lifecycle_events() {
    let mut state = state();
    let operation_id = SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 3,
    ])
    .unwrap();

    let projection = |next_seq: u64, recoverable_operation_id: Option<SurfaceOperationId>| {
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(next_seq),
            session_id: Some("recovery-session".to_string()),
            title: "Recovery session".to_string(),
            usage_revision: next_seq,
            usage: UsageTotals::default(),
            context_revision: next_seq,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: recoverable_operation_id.clone(),
            recoverable_operation_id,
            goal_presentation: None,
            session_presentation: None,
        }
    };

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        1,
        Some(operation_id.clone()),
    ))));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_id));
    assert!(state.recovery_prompt_visible);

    state.update(TuiEvent::TurnStarted {
        turn: 2,
        task: None,
    });
    assert_eq!(state.recoverable_operation_id(), Some(&operation_id));

    state.update(TuiEvent::HistoryLoaded {
        messages: Vec::new(),
        plan: None,
        label: "Resumed saved conversation.".to_string(),
    });
    state.update(TuiEvent::SessionCompleted {
        status: "cancelled".to_string(),
    });
    assert_eq!(state.recoverable_operation_id(), Some(&operation_id));
    assert!(state.recovery_prompt_visible);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        2, None,
    ))));
    assert!(state.recoverable_operation_id().is_none());
    assert!(!state.recovery_prompt_visible);
}

#[test]
fn surface_operation_projection_fences_conflicts_and_resets() {
    let mut state = state();
    let operation_a = SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 4,
    ])
    .unwrap();
    let operation_b = SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 5,
    ])
    .unwrap();
    let projection =
        |next_seq: u64, operation: Option<SurfaceOperationId>| SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(next_seq),
            session_id: Some("operation-session".to_string()),
            title: "Operation session".to_string(),
            usage_revision: next_seq,
            usage: UsageTotals::default(),
            context_revision: next_seq,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: operation.clone(),
            recoverable_operation_id: operation,
            goal_presentation: None,
            session_presentation: None,
        };
    let recovery_notice_count = |state: &AppState| {
        state.transcript.messages
                .iter()
                .filter(|message| {
                    matches!(message, ChatMessage::System(text) if text.starts_with("A recoverable operation is suspended."))
                })
                .count()
    };

    let accepted = projection(1, Some(operation_a.clone()));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        accepted.clone(),
    )));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(accepted)));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
    assert_eq!(recovery_notice_count(&state), 1);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        1,
        Some(operation_b.clone()),
    ))));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
    assert_eq!(recovery_notice_count(&state), 1);

    let mut mismatched = projection(2, Some(operation_b.clone()));
    mismatched.foreground_operation_id = Some(operation_a.clone());
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(mismatched)));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
    assert_eq!(recovery_notice_count(&state), 1);

    let mut mismatched_reset = projection(2, Some(operation_b.clone()));
    mismatched_reset.foreground_operation_id = Some(operation_a.clone());
    mismatched_reset.session_id = Some("invalid-reset-session".to_string());
    state.update(TuiEvent::SessionProjectionReset(Box::new(mismatched_reset)));
    assert_eq!(state.current_session_id(), Some("operation-session"));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
    assert_eq!(recovery_notice_count(&state), 1);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        2,
        Some(operation_b.clone()),
    ))));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_b));
    assert_eq!(recovery_notice_count(&state), 2);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        3, None,
    ))));
    assert!(state.recoverable_operation_id().is_none());
    assert!(!state.recovery_prompt_visible);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        2,
        Some(operation_b.clone()),
    ))));
    assert!(state.recoverable_operation_id().is_none());

    let mut other_session = projection(4, Some(operation_b.clone()));
    other_session.cursor.incarnation = orca_runtime::surface::SurfaceIncarnation::try_from_bytes([
        0x02, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 6,
    ])
    .unwrap();
    other_session.session_id = Some("other-operation-session".to_string());
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        other_session.clone(),
    )));
    assert!(state.recoverable_operation_id().is_none());

    state.update(TuiEvent::SessionProjectionReset(Box::new(other_session)));
    assert_eq!(state.current_session_id(), Some("other-operation-session"));
    assert_eq!(state.recoverable_operation_id(), Some(&operation_b));
    assert!(state.recovery_prompt_visible);
}

#[test]
fn workflow_task_projection_fences_contradictory_equal_cursor() {
    let mut state = state();
    let projection = |tasks| SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(1),
        session_id: Some("workflow-task-session".to_string()),
        title: "Workflow task session".to_string(),
        usage_revision: 1,
        usage: UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: tasks,
        current_goal: None,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation: None,
        session_presentation: None,
    };
    let accepted = projection(vec![workflow_task_summary("task-a", "Accepted task")]);
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        accepted.clone(),
    )));

    let contradictory = projection(vec![workflow_task_summary("task-b", "Contradictory task")]);
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(contradictory)));

    assert_eq!(state.workflow_tasks(), accepted.workflow_tasks);
}

#[test]
fn usage_projection_allows_compaction_drop_and_rejects_stale_revision() {
    let mut state = state();
    let before_compaction = UsageTotals {
        input_tokens: 50_000,
        output_tokens: 800,
        cache_tokens: 400,
        estimated_cost_usd: 0.03,
    };
    let after_compaction = UsageTotals {
        input_tokens: 8_000,
        output_tokens: 900,
        cache_tokens: 450,
        estimated_cost_usd: 0.035,
    };
    let stale = UsageTotals {
        input_tokens: 60_000,
        output_tokens: 700,
        cache_tokens: 350,
        estimated_cost_usd: 0.025,
    };

    let projection = |usage_revision, usage| SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(usage_revision),
        session_id: Some("usage-session".to_string()),
        title: "Usage session".to_string(),
        usage_revision,
        usage,
        context_revision: 1,
        context_used_tokens: 8_000,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: None,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation: None,
        session_presentation: None,
    };

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        10,
        before_compaction,
    ))));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        11,
        after_compaction.clone(),
    ))));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        9, stale,
    ))));

    assert_eq!(state.usage(), &after_compaction);
}

#[test]
fn surface_session_projection_fences_stale_and_cross_thread_identity() {
    let mut state = state();
    let projection = |cursor, session_id: &str, title: &str| SurfaceProjectionState {
        cursor,
        session_id: Some(session_id.to_string()),
        title: title.to_string(),
        usage_revision: 1,
        usage: UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: None,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation: None,
        session_presentation: None,
    };
    let committed = projection(
        crate::surface_projection::test_surface_cursor(2),
        "session-1",
        "Committed title",
    );
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(committed)));

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(1),
        "session-1",
        "Stale title",
    ))));
    assert_eq!(
        state.current_session_title(),
        Some("Committed title"),
        "an older cursor must not overwrite the accepted title"
    );

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(2),
        "session-1",
        "Contradictory title",
    ))));
    assert_eq!(
        state.current_session_title(),
        Some("Committed title"),
        "an equal cursor with contradictory identity must be rejected"
    );

    let mut different_cursor = crate::surface_projection::test_surface_cursor(1);
    let mut thread_bytes = [9; 16];
    thread_bytes[6] = 0x79;
    thread_bytes[8] = 0x89;
    different_cursor.thread_id =
        orca_runtime::surface::SurfaceThreadId::try_from_bytes(thread_bytes)
            .expect("different test surface thread id");
    let mut different_projection = projection(different_cursor, "session-2", "Different thread");
    different_projection.usage.input_tokens = 99;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        different_projection.clone(),
    )));
    assert_eq!(
        state.current_session_id(),
        Some("session-1"),
        "ordinary projection cannot switch threads"
    );
    assert_eq!(
        state.usage().input_tokens,
        0,
        "rejected identity must reject the whole projection envelope"
    );

    state.update(TuiEvent::SessionProjectionReset(Box::new(
        different_projection,
    )));
    assert_eq!(state.current_session_id(), Some("session-2"));
    assert_eq!(state.current_session_title(), Some("Different thread"));
    assert_eq!(state.usage().input_tokens, 99);
}

#[test]
fn surface_session_projection_presents_once_per_cursor() {
    let mut state = state();
    let projection = |next_seq, title: &str, session_presentation| SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(next_seq),
        session_id: Some("session-1".to_string()),
        title: title.to_string(),
        usage_revision: 1,
        usage: UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: None,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation: None,
        session_presentation,
    };
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        1,
        "Initial title",
        None,
    ))));
    let renamed = projection(
        2,
        "Committed title",
        Some(crate::surface_projection::SessionProjectionPresentation::Renamed),
    );
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(renamed.clone())));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(renamed.clone())));
    let duplicate_directive = SurfaceProjectionState {
        session_presentation: Some(
            crate::surface_projection::SessionProjectionPresentation::Forked,
        ),
        ..renamed
    };
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        duplicate_directive,
    )));

    assert_eq!(state.current_session_title(), Some("Committed title"));
    assert_eq!(
        state
            .transcript
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    ChatMessage::System(text)
                        if text == "Renamed conversation to Committed title."
                )
            })
            .count(),
        1
    );
    assert!(!state.transcript.messages.iter().any(|message| {
        matches!(message, ChatMessage::System(text) if text.starts_with("Forked conversation"))
    }));
}

#[test]
fn rejected_reset_preserves_existing_surface_state() {
    let mut state = state();
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("session-1".to_string()),
            title: "Existing title".to_string(),
            usage_revision: 1,
            usage: UsageTotals {
                input_tokens: 7,
                ..UsageTotals::default()
            },
            context_revision: 1,
            context_used_tokens: 3,
            context_limit_tokens: 100,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        },
    )));
    state.push_message(ChatMessage::User("keep me".to_string()));

    state.update(TuiEvent::SessionProjectionReset(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(2),
            session_id: None,
            title: "Invalid ephemeral rename".to_string(),
            usage_revision: 2,
            usage: UsageTotals::default(),
            context_revision: 2,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: Some(
                crate::surface_projection::SessionProjectionPresentation::Renamed,
            ),
        },
    )));

    assert_eq!(state.current_session_id(), Some("session-1"));
    assert_eq!(state.current_session_title(), Some("Existing title"));
    assert_eq!(state.usage().input_tokens, 7);
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [ChatMessage::User(prompt)] if prompt == "keep me"
    ));
}

#[test]
fn surface_goal_projection_rejects_equal_usage_stale_snapshot() {
    let mut state = state();
    let committed = ThreadGoal {
        session_id: "goal-session".to_string(),
        objective: "new objective".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Paused,
        token_budget: Some(10_000),
        tokens_used: 100,
        time_used_seconds: 10,
        created_at: 1,
        updated_at: 3,
    };
    let stale = ThreadGoal {
        objective: "old objective".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        tokens_used: 10,
        time_used_seconds: 1,
        updated_at: 2,
        ..committed.clone()
    };
    let projection = |cursor, goal, goal_presentation| SurfaceProjectionState {
        cursor,
        session_id: Some("goal-session".to_string()),
        title: "Goal session".to_string(),
        usage_revision: 1,
        usage: UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: goal,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation,
        session_presentation: None,
    };

    let committed_projection = projection(
        crate::surface_projection::test_surface_cursor(2),
        Some(committed.clone()),
        Some(crate::surface_projection::GoalProjectionPresentation::Updated),
    );
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        committed_projection.clone(),
    )));
    let goal_notice_count = |state: &AppState| {
        state.transcript.messages
                .iter()
                .filter(|message| {
                    matches!(message, ChatMessage::System(text) if text.contains("new objective"))
                })
                .count()
    };
    assert_eq!(goal_notice_count(&state), 1);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        committed_projection.clone(),
    )));
    assert_eq!(goal_notice_count(&state), 1, "equal replay is silent");

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(2),
        Some(stale.clone()),
        Some(crate::surface_projection::GoalProjectionPresentation::Updated),
    ))));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(1),
        Some(stale.clone()),
        None,
    ))));

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(3),
        Some(committed.clone()),
        None,
    ))));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        crate::surface_projection::test_surface_cursor(2),
        Some(stale.clone()),
        Some(crate::surface_projection::GoalProjectionPresentation::Updated),
    ))));

    let mut different_incarnation = crate::surface_projection::test_surface_cursor(4);
    let mut incarnation_bytes = [5; 16];
    incarnation_bytes[6] = 0x75;
    incarnation_bytes[8] = 0x85;
    different_incarnation.incarnation =
        orca_runtime::surface::SurfaceIncarnation::try_from_bytes(incarnation_bytes)
            .expect("different test surface incarnation");
    let after_reset = ThreadGoal {
        objective: "accepted after reset".to_string(),
        updated_at: 4,
        ..stale
    };
    let different_incarnation_projection = projection(
        different_incarnation,
        Some(after_reset.clone()),
        Some(crate::surface_projection::GoalProjectionPresentation::Updated),
    );
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        different_incarnation_projection.clone(),
    )));

    assert_eq!(state.current_goal(), Some(&committed));
    assert_eq!(goal_notice_count(&state), 1);

    state.update(TuiEvent::SessionProjectionReset(Box::new(
        different_incarnation_projection,
    )));
    assert_eq!(state.current_goal(), Some(&after_reset));
}

#[test]
fn surface_goal_projection_hydration_is_silent() {
    let mut state = state();
    let goal = ThreadGoal {
        session_id: "goal-session".to_string(),
        objective: "hydrate without a notice".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Paused,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 1,
        created_at: 1,
        updated_at: 2,
    };
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("goal-session".to_string()),
            title: "Goal session".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: Some(goal.clone()),
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        },
    )));

    assert_eq!(state.current_goal(), Some(&goal));
    assert!(state.transcript.messages.is_empty());
}

#[test]
fn surface_goal_projection_presents_clear_once_per_cursor() {
    let mut state = state();
    let goal = ThreadGoal {
        session_id: "goal-session".to_string(),
        objective: "clear me".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Paused,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 1,
        created_at: 1,
        updated_at: 2,
    };
    let projection = |next_seq, goal, goal_presentation| SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(next_seq),
        session_id: Some("goal-session".to_string()),
        title: "Goal session".to_string(),
        usage_revision: 1,
        usage: UsageTotals::default(),
        context_revision: 1,
        context_used_tokens: 0,
        context_limit_tokens: 128_000,
        workflow_tasks: Vec::new(),
        current_goal: goal,
        foreground_operation_id: None,
        recoverable_operation_id: None,
        goal_presentation,
        session_presentation: None,
    };
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
        1,
        Some(goal),
        None,
    ))));
    let cleared = projection(
        2,
        None,
        Some(crate::surface_projection::GoalProjectionPresentation::Cleared),
    );
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(cleared.clone())));
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(cleared)));

    assert!(state.current_goal().is_none());
    assert_eq!(
        state
            .transcript
            .messages
            .iter()
            .filter(|message| {
                matches!(message, ChatMessage::System(text) if text == "Goal cleared.")
            })
            .count(),
        1
    );
}

#[test]
fn surface_projection_consistency_current_goal_reconciles_session_scoped_state() {
    let mut state = state();
    state.replace_session_identity_for_test(
        Some("stale-session".to_string()),
        Some("stale title".to_string()),
    );
    state.update(TuiEvent::ToolRequested {
        id: "tool-1".to_string(),
        name: "shell".to_string(),
        target: None,
    });

    let operation_id = SurfaceOperationId::try_from_bytes([
        0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1,
    ])
    .expect("valid surface operation id");
    let goal = ThreadGoal {
        session_id: "canonical-session".to_string(),
        objective: "keep the projection canonical".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: Some(10_000),
        tokens_used: 42,
        time_used_seconds: 3,
        created_at: 1,
        updated_at: 2,
    };
    let expected = SurfaceProjectionState {
        cursor: crate::surface_projection::test_surface_cursor(7),
        session_id: Some("canonical-session".to_string()),
        title: "canonical title".to_string(),
        usage_revision: 7,
        usage: UsageTotals {
            input_tokens: 700,
            output_tokens: 70,
            cache_tokens: 7,
            estimated_cost_usd: 0.007,
        },
        context_revision: 1,
        context_used_tokens: 700,
        context_limit_tokens: 1_000,
        workflow_tasks: vec![workflow_task_summary("task-1", "Canonical task")],
        current_goal: Some(goal),
        foreground_operation_id: Some(operation_id),
        recoverable_operation_id: None,
        goal_presentation: None,
        session_presentation: None,
    };

    state.update(TuiEvent::SessionProjectionReset(Box::new(expected.clone())));

    assert_eq!(state.current_session_id(), Some("canonical-session"));
    assert_eq!(state.current_session_title(), Some("canonical title"));
    assert_eq!(state.usage(), &expected.usage);
    assert_eq!(state.context_used_tokens(), expected.context_used_tokens);
    assert_eq!(state.context_limit_tokens(), expected.context_limit_tokens);
    assert_eq!(state.workflow_tasks(), expected.workflow_tasks);
    assert_eq!(state.current_goal(), expected.current_goal.as_ref());
    assert_eq!(
        state.foreground_operation_id(),
        expected.foreground_operation_id.as_ref()
    );
    state.assert_surface_projection_consistent(&expected);

    let mut same_context_revision = expected.clone();
    same_context_revision.context_used_tokens = 25_000;
    same_context_revision.context_limit_tokens = 1_000_000;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        same_context_revision,
    )));
    assert_eq!(state.context_used_tokens(), expected.context_used_tokens);
    assert_eq!(state.context_limit_tokens(), expected.context_limit_tokens);

    let mut compacted = expected.clone();
    compacted.context_revision = 2;
    compacted.context_used_tokens = 10_000;
    compacted.context_limit_tokens = 1_000_000;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        compacted.clone(),
    )));
    assert_eq!(state.context_used_tokens(), 10_000);
    assert_eq!(state.context_limit_tokens(), 1_000_000);

    let mut newer_usage_with_stale_context = compacted;
    newer_usage_with_stale_context.usage_revision = 8;
    newer_usage_with_stale_context.usage.input_tokens = 800;
    newer_usage_with_stale_context.context_revision = 1;
    newer_usage_with_stale_context.context_used_tokens = 50_000;
    newer_usage_with_stale_context.context_limit_tokens = 128_000;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        newer_usage_with_stale_context,
    )));
    assert_eq!(state.usage().input_tokens, 800);
    assert_eq!(state.context_used_tokens(), 10_000);
    assert_eq!(state.context_limit_tokens(), 1_000_000);

    let mut stale = expected.clone();
    stale.usage_revision = 6;
    stale.title = "stale projection title".to_string();
    stale.usage.input_tokens = 1;
    stale.foreground_operation_id = None;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(stale)));
    assert_eq!(state.current_session_title(), Some("canonical title"));
    assert_eq!(state.usage().input_tokens, 800);
    assert_eq!(
        state.foreground_operation_id(),
        expected.foreground_operation_id.as_ref()
    );

    let mut next_session = expected.clone();
    next_session.session_id = Some("next-session".to_string());
    next_session.title = "next title".to_string();
    next_session.usage_revision = 1;
    next_session.usage.input_tokens = 12;
    next_session.context_revision = 1;
    next_session.context_used_tokens = 12_000;
    next_session.context_limit_tokens = 256_000;
    next_session.workflow_tasks.clear();
    next_session.current_goal = None;
    next_session.foreground_operation_id = None;
    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(next_session)));
    assert_eq!(state.usage().input_tokens, 800);

    let mut empty = expected;
    empty.cursor = crate::surface_projection::test_surface_cursor(9);
    empty.session_id = Some("empty-session".to_string());
    empty.title = "empty title".to_string();
    empty.usage_revision = 1;
    empty.usage = UsageTotals::default();
    empty.context_revision = 1;
    empty.context_used_tokens = 0;
    empty.context_limit_tokens = 0;
    empty.workflow_tasks.clear();
    empty.current_goal = None;
    empty.foreground_operation_id = None;
    state.update(TuiEvent::SessionProjectionReset(Box::new(empty)));
    assert_eq!(state.usage(), &UsageTotals::default());
    assert_eq!(state.context_used_tokens(), 0);
    assert_eq!(state.context_limit_tokens(), 0);
}

#[test]
fn backtrack_clamps_flushed_watermark_too() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("first".to_string()));
    state
        .transcript
        .messages
        .push(ChatMessage::Assistant("reply".to_string()));
    state.transcript.flushed_count = 2;
    state.transcript.finalized_count = 2;

    state
        .transcript
        .messages
        .push(ChatMessage::User("second".to_string()));
    state
        .transcript
        .messages
        .push(ChatMessage::Assistant("reply2".to_string()));
    state.remove_after_last_user();

    assert!(state.transcript.flushed_count <= state.transcript.messages.len());
    assert_eq!(state.transcript.messages.len(), 2);
}

#[test]
fn completed_tool_event_preserves_result_kind() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "grep-1".to_string(),
        name: "grep".to_string(),
        target: Some("needle".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "grep-1".to_string(),
        name: "grep".to_string(),
        status: "completed".to_string(),
        output: "(no matches)".to_string(),
        diff: None,
        kind: Some("no_matches".to_string()),
    });

    match &state.transcript.messages[0] {
        ChatMessage::ToolCall { kind, .. } => {
            assert_eq!(kind.as_deref(), Some("no_matches"));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn tool_call_index_matches_canonical_scan_after_mutations() {
    fn tool_call(id: &str) -> ChatMessage {
        ChatMessage::ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            target: None,
            status: "completed".to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        }
    }

    fn assert_matches_canonical_scan(state: &AppState, ids: &[&str]) {
        state.assert_tool_call_index_consistent();
        for id in ids {
            let canonical = state.transcript.messages.iter().position(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == id)
                });
            assert_eq!(state.tool_call_message_index(id), canonical, "id={id}");
        }
    }

    let mut state = state();
    state.push_message(tool_call("first"));
    state.push_message(ChatMessage::System("between".to_string()));
    state.push_message(tool_call("duplicate"));
    state.push_message(tool_call("duplicate"));
    assert_matches_canonical_scan(&state, &["first", "duplicate", "missing"]);

    assert!(state.replace_message(0, tool_call("replacement")));
    assert_matches_canonical_scan(&state, &["first", "replacement", "duplicate", "missing"]);

    state.truncate_messages(3);
    assert_matches_canonical_scan(&state, &["replacement", "duplicate"]);

    state.retain_messages(|message| !matches!(message, ChatMessage::System(_)));
    assert_matches_canonical_scan(&state, &["replacement", "duplicate"]);

    state.replace_messages([tool_call("history"), tool_call("history")]);
    assert_matches_canonical_scan(&state, &["replacement", "history"]);

    state.clear_messages();
    assert_matches_canonical_scan(&state, &["history"]);
}

#[test]
fn tool_output_delta_updates_matching_tool_id() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "a".to_string(),
        name: "bash".to_string(),
        target: Some("first".to_string()),
    });
    state.update(TuiEvent::ToolRequested {
        id: "b".to_string(),
        name: "bash".to_string(),
        target: Some("second".to_string()),
    });
    state.update(TuiEvent::ToolOutputDelta {
        id: "a".to_string(),
        chunk: "one\n".to_string(),
    });

    match &state.transcript.messages[0] {
        ChatMessage::ToolCall { output, .. } => {
            assert_eq!(output.as_deref(), Some("one\n"));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    match &state.transcript.messages[1] {
        ChatMessage::ToolCall { output, .. } => assert!(output.is_none()),
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn completed_tool_event_replaces_live_preview_with_canonical_output() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "bash-preview".to_string(),
        name: "bash".to_string(),
        target: Some("printf output".to_string()),
    });
    state.update(TuiEvent::ToolOutputDelta {
        id: "bash-preview".to_string(),
        chunk: "live preview".to_string(),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "bash-preview".to_string(),
        name: "bash".to_string(),
        status: "completed".to_string(),
        output: "canonical bounded output".to_string(),
        diff: None,
        kind: None,
    });

    match &state.transcript.messages[0] {
        ChatMessage::ToolCall { output, .. } => {
            assert_eq!(output.as_deref(), Some("canonical bounded output"));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn tool_call_progress_creates_and_updates_running_row() {
    let mut state = state();

    state.update(TuiEvent::ToolCallProgress {
        id: "call_1".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 12_345,
    });
    state.update(TuiEvent::ToolCallProgress {
        id: "call_1".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 24_690,
    });
    state.update(TuiEvent::ToolRequested {
        id: "call_1".to_string(),
        name: "write_file".to_string(),
        target: Some("big.js".to_string()),
    });

    assert_eq!(state.transcript.messages.len(), 1);
    match &state.transcript.messages[0] {
        ChatMessage::ToolCall {
            name,
            target,
            status,
            output,
            ..
        } => {
            assert_eq!(name, "write_file");
            assert_eq!(target.as_deref(), Some("big.js"));
            assert_eq!(status, "running");
            assert_eq!(output.as_deref(), Some("receiving arguments... 24.1 KB"));
        }
        other => panic!("expected tool progress row, got {other:?}"),
    }
}

#[test]
fn tool_call_progress_ignores_panel_owned_tools() {
    let mut state = state();

    state.update(TuiEvent::ToolCallProgress {
        id: "plan-1".to_string(),
        name: Some("update_plan".to_string()),
        arguments_bytes: 1024,
    });
    state.update(TuiEvent::ToolCallProgress {
        id: "subagent-1".to_string(),
        name: Some("subagent".to_string()),
        arguments_bytes: 2048,
    });

    assert!(state.transcript.messages.is_empty());
}

#[test]
fn terminal_events_remove_orphan_receiving_tool_progress() {
    let mut state = state();

    state.update(TuiEvent::ToolCallProgress {
        id: "call_1".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 12_345,
    });
    state.update(TuiEvent::Error("failed to parse tool call".to_string()));

    assert!(
        state.transcript.messages.iter().all(|message| {
            !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
        }),
        "error should clear orphan receiving rows: {:?}",
        state.transcript.messages
    );

    state.update(TuiEvent::ToolCallProgress {
        id: "call_2".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 24_690,
    });
    state.update(TuiEvent::SessionCompleted {
        status: "cancelled".to_string(),
    });

    assert!(
        state.transcript.messages.iter().all(|message| {
            !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
        }),
        "completion should clear orphan receiving rows: {:?}",
        state.transcript.messages
    );
}

#[test]
fn clearing_receiving_progress_preserves_finalized_prefix_boundaries() {
    let mut state = state();
    state.transcript.messages.push(ChatMessage::ToolCall {
        id: "frozen".to_string(),
        name: "write_file".to_string(),
        target: None,
        status: "receiving".to_string(),
        output: Some("receiving arguments... 1 KB".to_string()),
        diff: None,
        kind: None,
        expanded: false,
    });
    state.transcript.finalized_count = 1;
    state.transcript.flushed_count = 1;

    state.update(TuiEvent::ToolCallProgress {
        id: "live".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 24_690,
    });
    state.update(TuiEvent::Error("failed".to_string()));

    assert_eq!(state.transcript.finalized_count, 1);
    assert_eq!(state.transcript.flushed_count, 1);
    assert_eq!(state.transcript.messages.len(), 2);
    match &state.transcript.messages[0] {
        ChatMessage::ToolCall { id, status, .. } => {
            assert_eq!(id, "frozen");
            assert_eq!(status, "receiving");
        }
        other => panic!("finalized prefix should be preserved, got {other:?}"),
    }
    assert!(matches!(
        state.transcript.messages[1],
        ChatMessage::Error(_)
    ));
}

#[test]
fn toggle_latest_tool_output_flips_expanded_state() {
    let mut state = state();

    state.update(TuiEvent::ToolRequested {
        id: "tool-1".to_string(),
        name: "grep".to_string(),
        target: None,
    });

    assert!(state.toggle_latest_tool_output());
    match &state.transcript.messages[0] {
        ChatMessage::ToolCall { expanded, .. } => assert!(*expanded),
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn workflow_panel_state_defaults_to_empty() {
    let state = state();

    assert_eq!(state.panel_mode, PanelMode::Conversation);
    assert_eq!(state.workflow_selected_index(), 0);
    assert!(state.workflow_tasks().is_empty());
}

#[test]
fn show_workflows_preserves_available_selection() {
    let mut state = state();
    state.replace_workflow_tasks_for_test(vec![
        BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "demo".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        },
        workflow_task_summary("task-2", "repair"),
    ]);
    state.select_workflow_index_for_test(1);

    state.show_workflows();

    assert_eq!(state.panel_mode, PanelMode::Workflows);
    assert_eq!(state.workflow_selected_index(), 1);
}

#[test]
fn workflow_panel_selection_moves_within_available_tasks() {
    let mut state = state();
    state.replace_workflow_tasks_for_test(vec![
        workflow_task_summary("task-1", "audit"),
        workflow_task_summary("task-2", "repair"),
    ]);

    state.select_next_workflow_task();
    assert_eq!(state.workflow_selected_index(), 1);

    state.select_next_workflow_task();
    assert_eq!(state.workflow_selected_index(), 1);

    state.select_previous_workflow_task();
    assert_eq!(state.workflow_selected_index(), 0);

    state.replace_workflow_tasks_for_test(Vec::new());
    state.select_next_workflow_task();
    assert_eq!(state.workflow_selected_index(), 0);
}

#[test]
fn selected_background_approval_task_opens_approval_dialog() {
    let mut state = state();
    let mut task = workflow_task_summary("task-approval", "approval");
    task.task_type = TaskType::MainSession;
    task.status = TaskStatus::ApprovalRequired;
    task.is_backgrounded = true;
    task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
        id: "mock-tool-1".to_string(),
        name: "task_list".to_string(),
        action: orca_core::approval_types::ActionKind::Read,
        target: Some("background task".to_string()),
        arguments: "{\"limit\":1}".to_string(),
    });
    state.replace_workflow_tasks_for_test(vec![task]);

    assert!(state.open_selected_background_approval_dialog());

    assert_eq!(state.status, AppStatus::WaitingApproval);
    let dialog = state.approval_dialog.as_ref().expect("approval dialog");
    assert_eq!(dialog.tool, "task_list");
    assert_eq!(dialog.target.as_deref(), Some("background task"));
    assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
    assert_eq!(dialog.diff.as_deref(), Some("{\"limit\":1}"));
}

#[test]
fn foreground_claimed_background_approval_can_reopen_dialog() {
    let mut state = state();
    let mut task = workflow_task_summary("task-approval", "approval");
    task.task_type = TaskType::MainSession;
    task.status = TaskStatus::ApprovalRequired;
    task.is_backgrounded = false;
    task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
        id: "mock-tool-1".to_string(),
        name: "task_list".to_string(),
        action: orca_core::approval_types::ActionKind::Read,
        target: Some("foreground claimed task".to_string()),
        arguments: "{}".to_string(),
    });
    state.replace_workflow_tasks_for_test(vec![task]);

    assert!(state.open_selected_background_approval_dialog());
    assert_eq!(
        state
            .approval_dialog
            .as_ref()
            .and_then(|dialog| dialog.background_task_id.as_deref()),
        Some("task-approval")
    );
}

#[test]
fn show_agents_uses_dedicated_panel_mode() {
    let mut state = state();
    state.show_agents();

    assert_eq!(state.panel_mode, PanelMode::Agents);
    assert_eq!(state.workflow_selected_index(), 0);
}

#[test]
fn workflow_events_update_panel_and_queue_model_notification() {
    let mut state = state();
    state.apply_workflow_tasks_for_test(vec![BackgroundTaskSummary {
        id: "task-1".to_string(),
        task_type: TaskType::Workflow,
        status: TaskStatus::Completed,
        is_backgrounded: false,
        description: "demo".to_string(),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: Some(2_000),
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        pending_tool_call: None,
        name: Some("audit".to_string()),
        workflow_run_id: Some("workflow-run-1".to_string()),
        phase_count: Some(2),
        workflow_progress: None,
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        workflow_script_path: None,
        workflow_launch_input: None,
        workflow_final_summary: None,
        workflow_failure_count: 0,
        usage: None,
        subagent_current_activity: None,
        subagent_turn: None,
        last_activity_at_ms: None,
        continuation: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
        publication_revision: None,
    }]);
    state.update(TuiEvent::WorkflowNotification {
        id: "notification-1".to_string(),
        prompt: "<task-notification>done</task-notification>".to_string(),
        status: "completed".to_string(),
        summary: "audit: done".to_string(),
    });

    assert_eq!(state.workflow_tasks().len(), 1);
    assert_eq!(state.workflow_selected_index(), 0);
    let notification = state
        .pending_workflow_notifications
        .pop_front()
        .expect("pending workflow notification");
    assert_eq!(notification.id, "notification-1");
    assert_eq!(
        notification.prompt,
        "<task-notification>done</task-notification>"
    );
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::System(message)) if message.contains("Workflow completed. audit: done")
    ));
}

#[test]
fn duplicate_workflow_notification_id_is_not_queued_twice() {
    let mut state = state();

    state.update(TuiEvent::WorkflowNotification {
        id: "workflow-run-1:task-1:tool-1".to_string(),
        prompt: "<task-notification>done</task-notification>".to_string(),
        status: "completed".to_string(),
        summary: "audit: done".to_string(),
    });
    state.update(TuiEvent::WorkflowNotification {
        id: "workflow-run-1:task-1:tool-1".to_string(),
        prompt: "<task-notification>done again</task-notification>".to_string(),
        status: "completed".to_string(),
        summary: "audit: done again".to_string(),
    });

    assert_eq!(state.pending_workflow_notifications.len(), 1);
    assert_eq!(
        state.pending_workflow_notifications[0].prompt,
        "<task-notification>done</task-notification>"
    );
    let workflow_messages = state
        .transcript
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ChatMessage::System(text) if text.starts_with("Workflow completed.")
            )
        })
        .count();
    assert_eq!(workflow_messages, 1);
}

#[test]
fn pending_workflow_notification_queue_owns_unique_drain_and_notification_pop() {
    let queue = PendingWorkflowNotificationQueue::new();
    assert!(queue.push_unique(PendingWorkflowNotification {
        id: "notification-1".to_string(),
        prompt: "<task-notification>one</task-notification>".to_string(),
    }));
    assert!(!queue.push_unique(PendingWorkflowNotification {
        id: "notification-1".to_string(),
        prompt: "<task-notification>duplicate</task-notification>".to_string(),
    }));

    let mut pending = VecDeque::new();
    queue.drain_into(&mut pending);
    assert!(queue.is_empty());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "notification-1");

    assert!(queue.push_unique(PendingWorkflowNotification {
        id: "stale-notification".to_string(),
        prompt: "<task-notification>stale</task-notification>".to_string(),
    }));
    queue.clear();
    assert!(queue.is_empty());

    assert!(queue.push_unique(PendingWorkflowNotification {
        id: "notification-2".to_string(),
        prompt: "<task-notification>two</task-notification>".to_string(),
    }));
    assert_eq!(
        queue
            .pop_notification()
            .as_ref()
            .map(|notification| (notification.id.as_str(), notification.prompt.as_str())),
        Some((
            "notification-2",
            "<task-notification>two</task-notification>"
        ))
    );
    assert!(queue.pop_notification().is_none());
}

#[test]
fn workflow_task_updates_sort_actionable_active_then_recent_terminal_tasks() {
    let mut state = state();
    let mut completed = workflow_task_summary("task-completed", "completed");
    completed.status = TaskStatus::Completed;
    completed.completed_at_ms = Some(9_000);
    completed.last_activity_at_ms = Some(9_000);

    let mut running = workflow_task_summary("task-running", "running");
    running.status = TaskStatus::Running;
    running.last_activity_at_ms = Some(5_000);

    let mut approval = workflow_task_summary("task-approval", "approval");
    approval.task_type = TaskType::MainSession;
    approval.status = TaskStatus::ApprovalRequired;
    approval.is_backgrounded = true;
    approval.last_activity_at_ms = Some(1_000);

    state.apply_workflow_tasks_for_test(vec![completed, running, approval]);

    assert_eq!(
        state
            .workflow_tasks()
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-approval", "task-running", "task-completed"]
    );
}

#[test]
fn workflow_task_updates_preserve_selected_task_id_after_sorting() {
    let mut state = state();
    let mut running = workflow_task_summary("task-running", "running");
    running.status = TaskStatus::Running;
    running.last_activity_at_ms = Some(5_000);
    let mut completed = workflow_task_summary("task-completed", "completed");
    completed.status = TaskStatus::Completed;
    completed.completed_at_ms = Some(9_000);
    completed.last_activity_at_ms = Some(9_000);
    state.replace_workflow_tasks_for_test(vec![running.clone(), completed.clone()]);
    state.select_workflow_index_for_test(1);

    running.last_activity_at_ms = Some(10_000);
    state.apply_workflow_tasks_for_test(vec![completed, running]);

    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-completed")
    );
}

#[test]
fn workflow_task_update_preserves_owner_selection() {
    let mut state = state();
    let mut running = workflow_task_summary("task-running", "running");
    running.status = TaskStatus::Running;
    running.last_activity_at_ms = Some(5_000);
    let mut completed = workflow_task_summary("task-completed", "completed");
    completed.status = TaskStatus::Completed;
    completed.completed_at_ms = Some(9_000);
    completed.last_activity_at_ms = Some(9_000);
    state.apply_workflow_tasks_for_test(vec![running.clone(), completed.clone()]);
    state.show_workflows();
    state.select_next_workflow_task();
    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-completed")
    );

    completed.status = TaskStatus::Failed;
    completed.last_activity_at_ms = Some(10_000);
    state.apply_workflow_tasks_for_test(vec![running.clone(), completed]);

    assert_eq!(
        state
            .workflow_tasks()
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-running", "task-completed"]
    );
    assert_eq!(
        state
            .selected_workflow_task()
            .map(|task| (task.id.as_str(), task.status)),
        Some(("task-completed", TaskStatus::Failed))
    );
}

#[test]
fn backgrounded_main_session_update_reveals_and_selects_task_panel_once() {
    let mut state = state();
    let mut backgrounded = workflow_task_summary("task-main", "backgrounded");
    backgrounded.task_type = TaskType::MainSession;
    backgrounded.status = TaskStatus::Running;
    backgrounded.is_backgrounded = true;
    backgrounded.last_activity_at_ms = Some(8_000);
    let mut workflow = workflow_task_summary("task-workflow", "workflow");
    workflow.status = TaskStatus::Running;
    workflow.last_activity_at_ms = Some(9_000);

    state.apply_workflow_tasks_for_test(vec![workflow.clone(), backgrounded.clone()]);

    assert_eq!(state.panel_mode, PanelMode::Workflows);
    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-main")
    );

    let selected = state
        .workflow_tasks()
        .iter()
        .position(|task| task.id == "task-workflow")
        .expect("workflow task remains visible");
    state.select_workflow_index_for_test(selected);
    backgrounded.last_activity_at_ms = Some(10_000);
    state.apply_workflow_tasks_for_test(vec![workflow, backgrounded]);

    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-workflow")
    );
}

#[test]
fn backgrounded_approval_update_reveals_and_selects_task_panel_once() {
    let mut state = state();
    let mut approval = workflow_task_summary("task-approval", "approval");
    approval.task_type = TaskType::MainSession;
    approval.status = TaskStatus::ApprovalRequired;
    approval.is_backgrounded = true;
    approval.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
        id: "approval-1".to_string(),
        name: "task_list".to_string(),
        action: orca_core::approval_types::ActionKind::Read,
        target: None,
        arguments: "{}".to_string(),
    });
    approval.last_activity_at_ms = Some(8_000);
    let mut workflow = workflow_task_summary("task-workflow", "workflow");
    workflow.status = TaskStatus::Running;
    workflow.last_activity_at_ms = Some(9_000);

    state.apply_workflow_tasks_for_test(vec![workflow.clone(), approval.clone()]);

    assert_eq!(state.panel_mode, PanelMode::Workflows);
    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-approval")
    );

    let selected = state
        .workflow_tasks()
        .iter()
        .position(|task| task.id == "task-workflow")
        .expect("workflow task remains visible");
    state.select_workflow_index_for_test(selected);
    approval.last_activity_at_ms = Some(10_000);
    state.apply_workflow_tasks_for_test(vec![workflow, approval]);

    assert_eq!(
        state.selected_workflow_task().map(|task| task.id.as_str()),
        Some("task-workflow")
    );
}

#[test]
fn backgrounded_main_session_suppresses_foreground_output_until_completion() {
    let mut state = state();
    state.apply_workflow_tasks_for_test(vec![BackgroundTaskSummary {
        id: "task-main".to_string(),
        task_type: TaskType::MainSession,
        status: TaskStatus::Running,
        is_backgrounded: true,
        description: "long answer".to_string(),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: None,
        command: None,
        agent_type: Some("main-session".to_string()),
        server: None,
        tool: None,
        pending_tool_call: None,
        name: None,
        workflow_run_id: None,
        phase_count: None,
        workflow_progress: None,
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        workflow_script_path: None,
        workflow_launch_input: None,
        workflow_final_summary: None,
        workflow_failure_count: 0,
        usage: None,
        subagent_current_activity: None,
        subagent_turn: None,
        last_activity_at_ms: None,
        continuation: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
        publication_revision: None,
    }]);

    state.update(TuiEvent::MessageDelta(
        "hidden background output".to_string(),
    ));
    assert!(state.transcript.messages.is_empty());

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    state.update(TuiEvent::TurnStarted {
        turn: 2,
        task: None,
    });
    state.update(TuiEvent::MessageDelta(
        "visible foreground output\n".to_string(),
    ));

    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Assistant(text)) if text == "visible foreground output\n"
    ));
}

#[test]
fn foregrounded_main_session_task_update_clears_output_suppression() {
    let mut state = state();
    state.suppress_background_main_session_output = true;

    let mut task = workflow_task_summary("task-main", "foregrounded");
    task.task_type = TaskType::MainSession;
    task.status = TaskStatus::Running;
    task.is_backgrounded = false;
    state.apply_workflow_tasks_for_test(vec![task]);

    assert!(!state.suppress_background_main_session_output);
}

#[test]
fn background_output_attach_clears_suppression_before_replayed_delta() {
    let mut state = state();
    state.panel_mode = PanelMode::Workflows;
    state.suppress_background_main_session_output = true;

    state.update(TuiEvent::BackgroundTaskOutputAttached {
        task_id: "task-main".to_string(),
    });
    state.update(TuiEvent::MessageDelta(
        "missing foreground suffix\n".to_string(),
    ));

    assert!(!state.suppress_background_main_session_output);
    assert_eq!(state.panel_mode, PanelMode::Conversation);
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Assistant(text)) if text == "missing foreground suffix\n"
    ));
}

#[test]
fn foregrounded_selected_main_session_returns_to_conversation_panel() {
    let mut state = state();
    state.panel_mode = PanelMode::Workflows;
    state.suppress_background_main_session_output = true;

    let mut selected = workflow_task_summary("task-main", "selected");
    selected.task_type = TaskType::MainSession;
    selected.status = TaskStatus::Running;
    selected.is_backgrounded = true;
    let mut other = workflow_task_summary("task-other", "other");
    other.status = TaskStatus::Running;
    state.replace_workflow_tasks_for_test(vec![selected.clone(), other.clone()]);
    state.select_workflow_index_for_test(0);

    selected.is_backgrounded = false;
    state.apply_workflow_tasks_for_test(vec![selected, other]);

    assert_eq!(state.panel_mode, PanelMode::Conversation);
    assert!(!state.suppress_background_main_session_output);
}

#[test]
fn backgrounded_main_session_completion_adds_system_notice() {
    let mut state = state();
    state.apply_workflow_tasks_for_test(vec![BackgroundTaskSummary {
        id: "task-main".to_string(),
        task_type: TaskType::MainSession,
        status: TaskStatus::Running,
        is_backgrounded: true,
        description: "long answer".to_string(),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: None,
        command: None,
        agent_type: Some("main-session".to_string()),
        server: None,
        tool: None,
        pending_tool_call: None,
        name: None,
        workflow_run_id: None,
        phase_count: None,
        workflow_progress: None,
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        workflow_script_path: None,
        workflow_launch_input: None,
        workflow_final_summary: None,
        workflow_failure_count: 0,
        usage: None,
        subagent_current_activity: None,
        subagent_turn: None,
        last_activity_at_ms: None,
        continuation: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
        publication_revision: None,
    }]);

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::System(message))
            if message == "Background session completed: success"
    ));
    assert_eq!(state.status, AppStatus::Idle);
}

#[test]
fn active_goal_projection_does_not_mark_running_app_idle() {
    let mut state = state();
    state.status = AppStatus::Running;
    let goal = ThreadGoal {
        session_id: "session-1".to_string(),
        objective: "keep going".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 1,
        created_at: 1,
        updated_at: 1,
    };

    state.update(TuiEvent::GoalStatus(Some(goal.clone())));
    assert_eq!(state.status, AppStatus::Running);

    state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("session-1".to_string()),
            title: "Goal session".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: Some(goal),
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: Some(crate::surface_projection::GoalProjectionPresentation::Updated),
            session_presentation: None,
        },
    )));
    assert_eq!(state.status, AppStatus::Running);
}

#[test]
fn goal_status_is_presentation_only() {
    let mut state = state();
    let committed = ThreadGoal {
        session_id: "session-1".to_string(),
        objective: "committed objective".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Paused,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 1,
        created_at: 1,
        updated_at: 2,
    };
    let queried = ThreadGoal {
        objective: "queried objective".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        updated_at: 3,
        ..committed.clone()
    };
    state.replace_current_goal_for_test(Some(committed.clone()));

    state.update(TuiEvent::GoalStatus(Some(queried)));

    assert_eq!(state.current_goal(), Some(&committed));
    assert!(state.transcript.messages.iter().any(
        |message| matches!(message, ChatMessage::System(text) if text.contains("queried objective"))
    ));
}

#[test]
fn goal_status_messages_compact_long_objectives() {
    let mut state = state();
    let objective = "目标内容很长".repeat(100);
    let goal = ThreadGoal {
        session_id: "session-1".to_string(),
        objective: objective.clone(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: Some(2_000),
        tokens_used: 1_500,
        time_used_seconds: 120,
        created_at: 1,
        updated_at: 1,
    };

    state.update(TuiEvent::GoalStatus(Some(goal)));

    let Some(ChatMessage::System(message)) = state.transcript.messages.last() else {
        panic!("goal status should add a system message");
    };
    assert!(message.starts_with("Goal active · 目标内容"));
    assert!(message.contains('…'));
    assert!(message.ends_with("2m · 1.5K/2K tok"));
    assert!(!message.contains(&objective));
}

#[test]
fn running_goal_does_not_repeat_unchanged_status_notice() {
    let mut state = state();
    state.status = AppStatus::Running;
    let goal = ThreadGoal {
        session_id: "session-1".to_string(),
        objective: "keep going".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 120,
        created_at: 1,
        updated_at: 1,
    };

    state.update(TuiEvent::GoalStatus(Some(goal.clone())));
    state.update(TuiEvent::GoalStatus(Some(goal)));

    assert_eq!(
            state.transcript.messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::System(text) if text.starts_with("Goal active")))
                .count(),
            1
        );
}

#[test]
fn idle_goal_refreshes_between_turns_do_not_repeat_unchanged_status_notice() {
    // Between auto-continuation turns the goal loop emits several GoalStatus
    // refreshes (pre-turn poll, usage accounting, post-turn poll) while the
    // app has already returned to Idle. They render an identical line, so the
    // transcript must collapse them to a single notice regardless of status.
    let mut state = state();
    state.status = AppStatus::Idle;
    let goal = ThreadGoal {
        session_id: "session-1".to_string(),
        objective: "keep going".to_string(),
        status: orca_core::goal_types::ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 10,
        time_used_seconds: 120,
        created_at: 1,
        updated_at: 1,
    };

    state.update(TuiEvent::GoalStatus(Some(goal.clone())));
    state.update(TuiEvent::GoalStatus(Some(goal.clone())));
    state.update(TuiEvent::GoalStatus(Some(goal)));

    assert_eq!(
            state.transcript.messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::System(text) if text.starts_with("Goal active")))
                .count(),
            1
        );
}

#[test]
fn compacted_event_explains_runtime_recovery_reason() {
    let mut state = state();
    state.status = AppStatus::Compacting;

    state.update(TuiEvent::Compacted {
        before_messages: 12,
        after_messages: 5,
        reason: "prompt_too_long_recovery".to_string(),
        strategy: "remote_summary".to_string(),
        collapsed_messages: 7,
        status_text: "compacted context after prompt-too-long".to_string(),
    });

    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::System(message))
            if message == "Compacted conversation context after prompt-too-long: 12 -> 5 messages (collapsed 7, remote_summary)."
    ));
    assert_eq!(state.status, AppStatus::Idle);
}

#[test]
fn compaction_lifecycle_sets_compacting_until_completion() {
    let mut state = state();

    state.update(TuiEvent::CompactionStarted);
    assert_eq!(state.status, AppStatus::Compacting);

    state.update(TuiEvent::Compacted {
        before_messages: 12,
        after_messages: 5,
        reason: "manual".to_string(),
        strategy: "manual".to_string(),
        collapsed_messages: 7,
        status_text: "compacted context manually".to_string(),
    });
    assert_eq!(state.status, AppStatus::Idle);
}

#[test]
fn running_timer_starts_and_stops_with_running_status() {
    let mut state = state();
    assert!(state.running_started_at.is_none());

    state.update(TuiEvent::TurnStarted {
        turn: 1,
        task: None,
    });
    assert!(state.running_started_at.is_some());

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert_eq!(state.status, AppStatus::Idle);
    assert!(state.running_started_at.is_none());
}

#[test]
fn approval_round_trip_preserves_running_timer() {
    let mut state = state();
    state.update(TuiEvent::TurnStarted {
        turn: 1,
        task: None,
    });
    let started_at = Instant::now() - std::time::Duration::from_secs(65);
    state.running_started_at = Some(started_at);

    state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
        tool: "bash".to_string(),
        target: Some("cargo test".to_string()),
        preview: None,
    });
    assert_eq!(state.status, AppStatus::WaitingApproval);
    assert_eq!(state.running_started_at, Some(started_at));

    state.enter_running();
    assert_eq!(state.status, AppStatus::Running);
    assert_eq!(state.running_started_at, Some(started_at));
}

const EDIT_DIFF: &str = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

fn configured_edit_state() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().expect("edit workspace");
    std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
    std::fs::write(directory.path().join("src/item.py"), "value = 2\n").expect("post-edit file");
    let mut state = state();
    state.configure_syntax_highlighting(
        directory.path().to_path_buf(),
        crate::syntax_highlight::SyntaxTheme::OneHalfDark,
        crate::terminal_capabilities::TerminalColorLevel::TrueColor,
    );
    (directory, state)
}

fn submit_live_edit(state: &mut AppState, id: &str, target: &str, diff: &str) {
    state.update(TuiEvent::ToolRequested {
        id: id.to_string(),
        name: "edit".to_string(),
        target: Some(target.to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: id.to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: format!("edited {target}"),
        diff: Some(diff.to_string()),
        kind: None,
    });
}

fn malformed_structural_diffs() -> Vec<(&'static str, String)> {
    vec![
        (
            "malformed-hunk-candidate",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ malformed coordinates @@
@@ -1 +1 @@ valid function context
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "metadata-before-first-hunk",
            "\
--- a/src/item.py
+++ b/src/item.py
arbitrary metadata
@@ -1 +1 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "zero-old-start",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -0 +1 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "zero-new-start",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +0 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "zero-width",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1,0 @@
@@ -1 +1 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "overflow",
            format!(
                "--- a/src/item.py\n+++ b/src/item.py\n@@ -{},2 +1,2 @@\n-old = 1\n-old = 2\n+value = 2\n+new = 2\n",
                usize::MAX
            ),
        ),
        (
            "duplicate",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
@@ -1 +1 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "backward",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-old = 3
+new = 3
@@ -1 +1 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "old-overlap",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1 @@
-old = 1
-old = 2
+new = 1
@@ -2 +2 @@
-old = 2
+value = 2
"
            .to_string(),
        ),
        (
            "new-overlap",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,2 @@
-old = 1
+new = 1
+value = 2
@@ -2 +2 @@
-old = 2
+value = 2
"
            .to_string(),
        ),
        (
            "reused-old-anchor",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1 @@
+first = 1
@@ -1 +3 @@
-value = 1
+value = 2
"
            .to_string(),
        ),
        (
            "reused-new-anchor",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,0 @@
-value = 1
@@ -3 +1 @@
-other = 3
+value = 2
"
            .to_string(),
        ),
        (
            "null-old-range",
            "\
--- /dev/null
+++ b/src/item.py
@@ -1,0 +1 @@
+value = 2
"
            .to_string(),
        ),
        (
            "null-new-range",
            "\
--- a/src/item.py
+++ /dev/null
@@ -1 +1,0 @@
-value = 1
"
            .to_string(),
        ),
        (
            "both-null",
            "\
--- /dev/null
+++ /dev/null
@@ -0,0 +1 @@
+value = 2
"
            .to_string(),
        ),
    ]
}

fn state_with_submitted_edit_job() -> (
    tempfile::TempDir,
    AppState,
    crate::edit_highlight_worker::EditHighlightJob,
) {
    let (directory, mut state) = configured_edit_state();
    submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
    let job = state
        .pending_edit_highlight_job("edit-1")
        .expect("pending edit highlight job");
    (directory, state, job)
}

fn ready_result(
    job: crate::edit_highlight_worker::EditHighlightJob,
) -> crate::edit_highlight_worker::EditHighlightResult {
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    let styles = crate::diff_highlight::RefinedDiffStyles::from([(
        1,
        vec![Span::styled(
            "value = 2".to_string(),
            Style::default().fg(Color::Magenta),
        )],
    )]);
    crate::edit_highlight_worker::EditHighlightResult {
        job,
        outcome: crate::edit_highlight_worker::EditHighlightOutcome::Ready {
            styles: Arc::new(styles),
        },
    }
}

#[cfg(unix)]
fn real_alias_edit_state() -> (
    tempfile::TempDir,
    AppState,
    crate::edit_highlight_worker::EditHighlightJob,
) {
    use std::os::unix::fs::symlink;

    let (directory, mut state) = configured_edit_state();
    let alias = directory.path().join("src/alias.py");
    symlink(directory.path().join("src/item.py"), &alias).expect("initial alias");
    let request = orca_core::tool_types::ToolRequest {
        id: "alias-edit".to_string(),
        name: orca_core::tool_types::ToolName::Edit,
        action: orca_core::approval_types::ActionKind::Write,
        target: Some("src/alias.py".to_string()),
        raw_arguments: Some(
            r#"{"path":"src/alias.py","old_text":"value = 2","new_text":"value = 3"}"#.to_string(),
        ),
    };
    let result = orca_tools::edit::execute(&request, directory.path());
    assert_eq!(
        result.status,
        orca_core::tool_types::ToolStatus::Completed,
        "symlink alias edit failed: {:?}",
        result.error
    );
    let preview = result
        .file_change_preview
        .as_deref()
        .expect("committed alias preview");
    let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview else {
        panic!("alias edit should produce unified diff");
    };
    state.update(TuiEvent::ToolRequested {
        id: request.id.clone(),
        name: "edit".to_string(),
        target: request.target.clone(),
    });
    state.update(TuiEvent::ToolCompleted {
        id: request.id,
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: result.output.unwrap_or_default(),
        diff: Some(diff.clone()),
        kind: None,
    });
    let job = state
        .pending_edit_highlight_job("alias-edit")
        .expect("real alias edit pending job");
    (directory, state, job)
}

#[cfg(unix)]
#[test]
fn real_edit_producer_keeps_symlink_alias_as_job_display_path() {
    let (directory, state, job) = real_alias_edit_state();

    assert_eq!(
        job.absolute_path,
        directory
            .path()
            .join("src/item.py")
            .canonicalize()
            .expect("canonical item path")
    );
    assert_eq!(job.display_path, "src/alias.py");
    assert_eq!(job.parsed.destination_path.as_deref(), Some("src/alias.py"));
    assert_eq!(state.pending_edit_highlight_count(), 1);
}

#[cfg(unix)]
#[test]
fn ready_result_applies_while_symlink_alias_identity_is_unchanged() {
    let (_directory, mut state, job) = real_alias_edit_state();

    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_some()
    );
}

#[test]
fn successful_live_edit_submits_one_versioned_highlight_job() {
    let (directory, mut state) = configured_edit_state();

    state.update(TuiEvent::ToolRequested {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
    });
    state.update(TuiEvent::ToolCompleted {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: "edited src/item.py".to_string(),
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
    });

    let job = state
        .pending_edit_highlight_job("edit-1")
        .expect("pending job");
    assert!(state.edit_highlight_needs_tick());
    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert_eq!(state.successful_edit_highlight_submit_count(), 1);
    assert_eq!(job.tool_id, "edit-1");
    assert_eq!(job.message_index, 0);
    assert_eq!(job.message_revision, state.transcript.message_revisions[0]);
    assert_eq!(
        job.syntax_theme_revision,
        crate::terminal_capabilities::syntax_style_revision(
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        )
    );
    assert_eq!(
        job.syntax_theme,
        crate::syntax_highlight::SyntaxTheme::OneHalfDark
    );
    assert_eq!(
        job.syntax_color_level,
        crate::terminal_capabilities::TerminalColorLevel::TrueColor
    );
    assert_eq!(
        job.absolute_path,
        directory
            .path()
            .join("src/item.py")
            .canonicalize()
            .expect("canonical target")
    );
    assert_eq!(job.display_path, "src/item.py");
    assert_eq!(
        job.parsed,
        crate::diff_highlight::parse_unified_diff(EDIT_DIFF)
    );
}

#[test]
fn completion_only_tool_row_has_no_target_and_submits_no_job() {
    let (_directory, mut state) = configured_edit_state();

    state.update(TuiEvent::ToolCompleted {
        id: "edit-1".to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: "edited src/item.py".to_string(),
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
    });

    assert!(matches!(
        state.transcript.messages.first(),
        Some(ChatMessage::ToolCall { target: None, .. })
    ));
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn replayed_history_messages_never_submit_jobs() {
    let (_directory, mut state) = configured_edit_state();
    let historical = ChatMessage::ToolCall {
        id: "historical-edit".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
        status: "completed".to_string(),
        output: None,
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
        expanded: false,
    };

    state.push_message(historical.clone());
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());

    state.replace_messages([historical]);
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn ineligible_live_edits_do_not_start_runtime_or_submit_jobs() {
    fn assert_ineligible(
        configure: impl FnOnce(&tempfile::TempDir, &mut AppState) -> (String, String, String),
    ) {
        let (directory, mut state) = configured_edit_state();
        let (status, target, diff) = configure(&directory, &mut state);
        state.update(TuiEvent::ToolRequested {
            id: "edit-ineligible".to_string(),
            name: "edit".to_string(),
            target: Some(target),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "edit-ineligible".to_string(),
            name: "edit".to_string(),
            status,
            output: String::new(),
            diff: Some(diff),
            kind: None,
        });
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    assert_ineligible(|_, _| {
        (
            "failed".to_string(),
            "src/item.py".to_string(),
            EDIT_DIFF.into(),
        )
    });
    assert_ineligible(|_, _| {
        (
            "cancelled".to_string(),
            "src/item.py".to_string(),
            EDIT_DIFF.into(),
        )
    });
    assert_ineligible(|_, _| {
        (
            "completed".to_string(),
            "src/item.py".to_string(),
            " \n".to_string(),
        )
    });
    assert_ineligible(|directory, _| {
        std::fs::write(directory.path().join("src/item.unknown"), "value = 2\n")
            .expect("unknown syntax file");
        (
            "completed".to_string(),
            "src/item.unknown".to_string(),
            EDIT_DIFF.replace("item.py", "item.unknown"),
        )
    });
    assert_ineligible(|_, _| {
        (
            "completed".to_string(),
            "src/item.py".to_string(),
            format!("{EDIT_DIFF}--- a/src/other.py\n+++ b/src/other.py\n@@ -1 +1 @@\n-a\n+b\n"),
        )
    });
    assert_ineligible(|directory, _| {
        std::fs::write(directory.path().join("src/item.py"), "").expect("empty post-edit file");
        (
            "completed".to_string(),
            "src/item.py".to_string(),
            "--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +0,0 @@\n-value = 1\n".to_string(),
        )
    });
    assert_ineligible(|directory, _| {
        let outside = directory.path().parent().unwrap().join("outside-item.py");
        std::fs::write(&outside, "value = 2\n").expect("outside file");
        (
            "completed".to_string(),
            "../outside-item.py".to_string(),
            EDIT_DIFF.replace("src/item.py", "../outside-item.py"),
        )
    });
    assert_ineligible(|directory, _| {
        std::fs::remove_file(directory.path().join("src/item.py")).expect("remove source file");
        std::fs::create_dir(directory.path().join("src/item.py")).expect("file-shaped directory");
        (
            "completed".to_string(),
            "src/item.py".to_string(),
            EDIT_DIFF.into(),
        )
    });
}

#[test]
fn live_edit_without_configured_workspace_submits_no_job() {
    let mut state = state();

    submit_live_edit(&mut state, "no-workspace", "src/item.py", EDIT_DIFF);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn incomplete_unified_diff_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let incomplete = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "incomplete", "src/item.py", incomplete);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn malformed_structures_fail_closed_across_parser_first_paint_and_app_state() {
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);

    for (id, diff) in malformed_structural_diffs() {
        let parsed = crate::diff_highlight::parse_unified_diff(&diff);
        assert!(parsed.has_malformed_hunk, "{id}");
        assert!(!parsed.is_structurally_valid(), "{id}");
        assert_eq!(parsed.raw_fallback.as_deref(), Some(diff.as_str()), "{id}");

        let rendered = crate::diff_highlight::render_parsed_diff(&parsed, &theme, None);
        assert_eq!(rendered.len(), diff.lines().count(), "{id}");
        for (raw_line, rendered_line) in diff.lines().zip(rendered) {
            assert_eq!(
                rendered_line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>(),
                format!("    {raw_line}"),
                "{id}"
            );
            assert_eq!(rendered_line.spans.len(), 1, "{id}: {raw_line:?}");
        }

        let (_directory, mut state) = configured_edit_state();
        submit_live_edit(&mut state, id, "src/item.py", &diff);
        assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
        assert!(!state.edit_highlight_runtime_started(), "{id}");
    }
}

#[test]
fn headerless_then_headered_diff_is_ambiguous_and_submits_no_job() {
    let (_directory, mut state) = configured_edit_state();
    let diff = "\
@@ -1 +1 @@
-value = 1
+value = 2
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-other = 3
+other = 4
";

    let parsed = crate::diff_highlight::parse_unified_diff(diff);
    assert!(parsed.is_structurally_valid());
    assert!(parsed.has_multiple_files);

    submit_live_edit(&mut state, "mixed-sections", "src/item.py", diff);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn extra_source_line_after_completed_hunk_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let malformed = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
+unexpected = 3
";

    submit_live_edit(&mut state, "malformed", "src/item.py", malformed);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn missing_file_header_pair_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let malformed = "\
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "missing-header", "src/item.py", malformed);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn valid_rename_diff_uses_destination_target_and_submits_job() {
    let (_directory, mut state) = configured_edit_state();
    let renamed = "\
--- a/src/old.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "rename", "src/item.py", renamed);

    let job = state
        .pending_edit_highlight_job("rename")
        .expect("rename destination job");
    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert_eq!(job.display_path, "src/item.py");
}

#[test]
fn valid_added_file_diff_uses_destination_target_and_submits_job() {
    let (_directory, mut state) = configured_edit_state();
    let added = "\
--- /dev/null
+++ b/src/item.py
@@ -0,0 +1 @@
+value = 2
";

    submit_live_edit(&mut state, "add", "src/item.py", added);

    let job = state
        .pending_edit_highlight_job("add")
        .expect("added file destination job");
    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert_eq!(job.display_path, "src/item.py");
}

#[test]
fn dev_null_requires_zero_start_and_zero_count() {
    let (_directory, mut state) = configured_edit_state();
    let invalid_add = "\
--- /dev/null
+++ b/src/item.py
@@ -1,0 +1 @@
+value = 2
";
    let invalid_delete = "\
--- a/src/item.py
+++ /dev/null
@@ -1 +1,0 @@
-value = 1
";

    submit_live_edit(&mut state, "invalid-null-add", "src/item.py", invalid_add);
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!parsed_diff_structure_matches_target(
        &crate::diff_highlight::parse_unified_diff(invalid_add),
        invalid_add,
        Path::new("src/item.py")
    ));
    assert!(!parsed_diff_structure_matches_target(
        &crate::diff_highlight::parse_unified_diff(invalid_delete),
        invalid_delete,
        Path::new("src/item.py")
    ));
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn in_workspace_parent_component_normalizes_and_submits_job() {
    let (directory, mut state) = configured_edit_state();
    std::fs::write(directory.path().join("item.py"), "value = 2\n")
        .expect("normalized post-edit file");
    let diff = "\
--- a/item.py
+++ b/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "normalized", "src/../item.py", diff);

    let job = state
        .pending_edit_highlight_job("normalized")
        .expect("normalized target job");
    assert_eq!(job.display_path, "item.py");
    assert_eq!(
        job.absolute_path,
        directory
            .path()
            .join("item.py")
            .canonicalize()
            .expect("canonical normalized target")
    );
}

#[cfg(unix)]
#[test]
fn real_edit_producer_lexically_normalizes_symlink_parent_target() {
    use std::os::unix::fs::symlink;

    let (directory, mut state) = configured_edit_state();
    let outside = tempfile::tempdir().expect("outside root");
    symlink(outside.path(), directory.path().join("link")).expect("outside symlink");
    let request = orca_core::tool_types::ToolRequest {
        id: "parent-edit".to_string(),
        name: orca_core::tool_types::ToolName::Edit,
        action: orca_core::approval_types::ActionKind::Write,
        target: Some("link/../src/item.py".to_string()),
        raw_arguments: Some(
            r#"{"path":"link/../src/item.py","old_text":"value = 2","new_text":"value = 3"}"#
                .to_string(),
        ),
    };
    let result = orca_tools::edit::execute(&request, directory.path());
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let preview = result
        .file_change_preview
        .as_deref()
        .expect("committed parent preview");
    let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview else {
        panic!("parent edit should produce unified diff");
    };
    state.update(TuiEvent::ToolRequested {
        id: request.id.clone(),
        name: "edit".to_string(),
        target: request.target.clone(),
    });
    state.update(TuiEvent::ToolCompleted {
        id: request.id,
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: result.output.unwrap_or_default(),
        diff: Some(diff.clone()),
        kind: None,
    });

    let job = state
        .pending_edit_highlight_job("parent-edit")
        .expect("parent edit pending job");
    assert_eq!(
        job.absolute_path,
        directory
            .path()
            .join("src/item.py")
            .canonicalize()
            .expect("canonical item")
    );
    assert_eq!(job.display_path, "src/item.py");
    assert_eq!(job.parsed.destination_path.as_deref(), Some("src/item.py"));
}

#[test]
fn real_edit_producer_allows_parent_reentry_into_same_workspace() {
    let (directory, mut state) = configured_edit_state();
    let workspace_name = directory
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("utf-8 workspace name");
    let target = format!("../{workspace_name}/src/item.py");
    let request = orca_core::tool_types::ToolRequest {
        id: "parent-reentry-edit".to_string(),
        name: orca_core::tool_types::ToolName::Edit,
        action: orca_core::approval_types::ActionKind::Write,
        target: Some(target.clone()),
        raw_arguments: Some(format!(
            r#"{{"path":"{target}","old_text":"value = 2","new_text":"value = 3"}}"#
        )),
    };
    let result = orca_tools::edit::execute(&request, directory.path());
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let preview = result
        .file_change_preview
        .as_deref()
        .expect("committed parent-reentry preview");
    let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview else {
        panic!("parent-reentry edit should produce unified diff");
    };
    let parsed = crate::diff_highlight::parse_unified_diff(diff);
    let expected_relative = PathBuf::from("src")
        .join("item.py")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        parsed.destination_path.as_deref(),
        Some(expected_relative.as_str())
    );
    state.update(TuiEvent::ToolRequested {
        id: request.id.clone(),
        name: "edit".to_string(),
        target: request.target.clone(),
    });
    state.update(TuiEvent::ToolCompleted {
        id: request.id,
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: result.output.unwrap_or_default(),
        diff: Some(diff.clone()),
        kind: None,
    });

    let job = state
        .pending_edit_highlight_job("parent-reentry-edit")
        .expect("parent-reentry pending job");
    assert_eq!(
        job.absolute_path,
        directory
            .path()
            .join("src/item.py")
            .canonicalize()
            .expect("canonical item")
    );
    assert_eq!(job.display_path, "src/item.py");
    assert_eq!(
        job.parsed.destination_path.as_deref(),
        Some(expected_relative.as_str())
    );
}

#[cfg(unix)]
#[test]
fn app_target_resolution_matches_tool_resolution_table() {
    use std::os::unix::fs::symlink;

    let (directory, state) = configured_edit_state();
    std::fs::write(directory.path().join("item.py"), "value = 2\n").expect("root item");
    symlink(
        directory.path().join("src/item.py"),
        directory.path().join("src/alias.py"),
    )
    .expect("alias symlink");
    let outside = tempfile::tempdir().expect("outside root");
    std::fs::create_dir(outside.path().join("child")).expect("outside child");
    std::fs::write(outside.path().join("escaped.py"), "value = 2\n").expect("outside file");
    symlink(
        outside.path().join("child"),
        directory.path().join("linked"),
    )
    .expect("outside child symlink");

    let workspace_name = directory
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("utf-8 workspace name");
    let parent_reentry = format!("../{workspace_name}/src/item.py");
    let cases = vec![
        ("src/item.py".to_string(), Some("src/item.py")),
        ("src/../item.py".to_string(), Some("item.py")),
        ("src/alias.py".to_string(), Some("src/alias.py")),
        (parent_reentry, Some("src/item.py")),
        ("../escaped.py".to_string(), None),
        ("linked/escaped.py".to_string(), None),
    ];

    for (target, expected_display) in cases {
        let tool_path = orca_tools::resolve_workspace_path(directory.path(), Some(&target))
            .ok()
            .filter(|path| path.is_file())
            .and_then(|path| path.canonicalize().ok());
        let app_path = state.resolve_edit_target(&target);
        assert_eq!(app_path.is_some(), tool_path.is_some(), "{target}");
        if let (Some((app_absolute, display)), Some(tool_absolute)) = (app_path, tool_path) {
            assert_eq!(app_absolute, tool_absolute, "{target}");
            assert_eq!(
                display,
                expected_display.expect("expected display"),
                "{target}"
            );
        }
    }
}

#[test]
fn reversed_file_headers_are_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let reversed = "\
+++ b/src/item.py
--- a/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "reversed", "src/item.py", reversed);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn arbitrary_in_hunk_and_trailing_metadata_are_rejected() {
    for (id, diff) in [
        (
            "leading-metadata",
            "\
arbitrary metadata
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
",
        ),
        (
            "in-hunk-metadata",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@
-value = 1
+value = 2
arbitrary metadata
 shared = 3
",
        ),
        (
            "trailing-metadata",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
arbitrary metadata
",
        ),
    ] {
        let (_directory, mut state) = configured_edit_state();

        submit_live_edit(&mut state, id, "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
        assert!(!state.edit_highlight_runtime_started(), "{id}");
    }
}

#[test]
fn standard_no_newline_metadata_and_header_timestamps_are_allowed() {
    let (_directory, mut state) = configured_edit_state();
    let diff = "\
--- a/src/item.py\t2026-07-24 10:00:00
+++ b/src/item.py\t2026-07-24 10:01:00
@@ -1 +1 @@
-value = 1
\\ No newline at end of file
+value = 2
\\ No newline at end of file
";

    submit_live_edit(&mut state, "standard-metadata", "src/item.py", diff);

    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert!(
        state
            .pending_edit_highlight_job("standard-metadata")
            .is_some()
    );
}

#[test]
fn zero_new_side_coordinate_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let invalid = "\
--- a/src/item.py
+++ b/src/item.py
@@ -0,0 +0,1 @@
+value = 2
";

    submit_live_edit(&mut state, "zero-new-line", "src/item.py", invalid);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn positive_count_with_zero_start_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let invalid = "\
--- a/src/item.py
+++ b/src/item.py
@@ -0 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "zero-old-start", "src/item.py", invalid);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn positive_start_zero_count_mid_file_insertion_is_eligible() {
    let (directory, mut state) = configured_edit_state();
    std::fs::write(
        directory.path().join("src/item.py"),
        "first = 1\nvalue = 2\n",
    )
    .expect("post-insert file");
    let insertion = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +2 @@ fn context
+value = 2
";

    submit_live_edit(&mut state, "mid-insert", "src/item.py", insertion);

    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert!(state.pending_edit_highlight_job("mid-insert").is_some());
}

#[test]
fn empty_zero_width_hunk_is_rejected_before_runtime_spawn() {
    let (_directory, mut state) = configured_edit_state();
    let diff = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1,0 @@
@@ -1 +1 @@
-value = 1
+value = 2
";

    submit_live_edit(&mut state, "empty-hunk", "src/item.py", diff);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn duplicate_backward_and_overlapping_hunks_are_rejected() {
    let cases = [
        (
            "duplicate-hunk",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-old = 1
+value = 2
@@ -1 +1 @@
-old = 1
+value = 2
",
        ),
        (
            "backward-hunk",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-old = 3
+new = 3
@@ -1 +1 @@
-old = 1
+value = 2
",
        ),
        (
            "old-overlap",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1 @@
-old = 1
-old = 2
+new = 1
@@ -2 +2 @@
-old = 2
+value = 2
",
        ),
        (
            "new-overlap",
            "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,2 @@
-old = 1
+new = 1
+value = 2
@@ -2 +2 @@
-old = 2
+value = 2
",
        ),
    ];

    for (id, diff) in cases {
        let (_directory, mut state) = configured_edit_state();

        submit_live_edit(&mut state, id, "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
        assert!(!state.edit_highlight_runtime_started(), "{id}");
    }
}

#[test]
fn overflowing_hunk_endpoint_is_rejected() {
    let (_directory, mut state) = configured_edit_state();
    let diff = format!(
        "--- a/src/item.py\n+++ b/src/item.py\n@@ -{},2 +1,2 @@\n-old = 1\n-old = 2\n+value = 2\n+new = 2\n",
        usize::MAX
    );

    submit_live_edit(&mut state, "overflowing-hunk", "src/item.py", &diff);

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn two_non_overlapping_hunks_with_function_context_are_eligible() {
    let (directory, mut state) = configured_edit_state();
    std::fs::write(
        directory.path().join("src/item.py"),
        "first = 1\nvalue = 2\nthird = 3\nvalue = 4\n",
    )
    .expect("two-hunk post-edit file");
    let diff = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@ first section
 first = 1
-old = 1
+value = 2
@@ -3,2 +3,2 @@ second section
 third = 3
-old = 2
+value = 4
";

    submit_live_edit(&mut state, "two-hunks", "src/item.py", diff);

    assert_eq!(state.pending_edit_highlight_count(), 1);
    assert!(state.pending_edit_highlight_job("two-hunks").is_some());
}

#[test]
fn dev_null_counts_correlate_and_delete_only_stays_ineligible() {
    let (_directory, mut state) = configured_edit_state();
    let malformed_add = "\
--- /dev/null
+++ b/src/item.py
@@ -1 +1 @@
-old
+value = 2
";
    let delete = "\
--- a/src/item.py
+++ /dev/null
@@ -1 +0,0 @@
-value = 1
";

    submit_live_edit(&mut state, "malformed-add", "src/item.py", malformed_add);
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!parsed_diff_structure_matches_target(
        &crate::diff_highlight::parse_unified_diff(malformed_add),
        malformed_add,
        Path::new("src/item.py")
    ));

    assert!(parsed_diff_structure_matches_target(
        &crate::diff_highlight::parse_unified_diff(delete),
        delete,
        Path::new("src/item.py")
    ));
    submit_live_edit(&mut state, "delete", "src/item.py", delete);
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn absolute_empty_mismatch_and_symlink_escape_targets_are_rejected() {
    let (directory, mut state) = configured_edit_state();
    let absolute = directory.path().join("src/item.py");
    submit_live_edit(
        &mut state,
        "absolute",
        absolute.to_str().expect("utf-8 absolute path"),
        EDIT_DIFF,
    );
    assert!(state.pending_edit_highlight_job("absolute").is_none());
    assert!(!state.edit_highlight_runtime_started());

    submit_live_edit(&mut state, "empty", "", EDIT_DIFF);
    assert!(state.pending_edit_highlight_job("empty").is_none());
    assert!(!state.edit_highlight_runtime_started());

    submit_live_edit(
        &mut state,
        "mismatch",
        "src/item.py",
        &EDIT_DIFF.replace("src/item.py", "src/other.py"),
    );
    assert!(state.pending_edit_highlight_job("mismatch").is_none());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let alias_parent = tempfile::tempdir().expect("workspace alias parent");
        let workspace_alias = alias_parent.path().join("workspace");
        symlink(directory.path(), &workspace_alias).expect("workspace symlink");
        state.configure_syntax_highlighting(
            workspace_alias,
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        );
        let outside = tempfile::tempdir().expect("outside directory");
        std::fs::write(outside.path().join("escaped.py"), "value = 2\n").expect("outside file");
        symlink(outside.path(), directory.path().join("linked")).expect("outside symlink");
        submit_live_edit(
            &mut state,
            "symlink-ancestor",
            "linked/escaped.py",
            &EDIT_DIFF.replace("src/item.py", "linked/escaped.py"),
        );
        assert!(!state.edit_highlight_runtime_started());
    }
}

#[test]
fn targetless_tool_request_and_completion_submit_no_job() {
    let (_directory, mut state) = configured_edit_state();
    state.update(TuiEvent::ToolRequested {
        id: "targetless".to_string(),
        name: "edit".to_string(),
        target: None,
    });
    state.update(TuiEvent::ToolCompleted {
        id: "targetless".to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: "edited".to_string(),
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
    });

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn completion_with_only_settled_reused_id_pushes_targetless_row() {
    for old_status in ["completed", "failed"] {
        let (_directory, mut state) = configured_edit_state();
        state.push_message(ChatMessage::ToolCall {
            id: "reused".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
            status: old_status.to_string(),
            output: Some("old output".to_string()),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
            expanded: false,
        });

        state.update(TuiEvent::ToolCompleted {
            id: "reused".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "new output".to_string(),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
        });

        assert_eq!(state.transcript.messages.len(), 2, "{old_status}");
        assert!(matches!(
            &state.transcript.messages[0],
            ChatMessage::ToolCall {
                target: Some(target),
                status,
                output: Some(output),
                ..
            } if target == "src/item.py" && status == old_status && output == "old output"
        ));
        assert!(matches!(
            &state.transcript.messages[1],
            ChatMessage::ToolCall {
                target: None,
                status,
                output: Some(output),
                ..
            } if status == "completed" && output == "new output"
        ));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }
}

#[test]
fn injected_worker_spawn_failure_is_silent_and_leaves_no_pending_state() {
    fn fail_runtime() -> std::io::Result<crate::edit_highlight_worker::EditHighlightRuntime> {
        Err(std::io::Error::other("injected spawn failure"))
    }

    let (_directory, mut state) = configured_edit_state();
    state.set_edit_highlight_runtime_factory_for_test(fail_runtime);
    submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);

    assert_eq!(state.transcript.messages.len(), 1);
    assert!(matches!(
        state.transcript.messages.first(),
        Some(ChatMessage::ToolCall { id, .. }) if id == "edit-1"
    ));
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}

#[test]
fn exact_ready_result_touches_only_matching_message_and_stores_arc_map() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    state.push_message(ChatMessage::System("unrelated".to_string()));
    let revisions_before = state.transcript.message_revisions.clone();
    let result = ready_result(job.clone());
    let expected_styles = match &result.outcome {
        crate::edit_highlight_worker::EditHighlightOutcome::Ready { styles } => Arc::clone(styles),
        crate::edit_highlight_worker::EditHighlightOutcome::Failed => unreachable!(),
    };

    assert!(state.apply_edit_highlight_result(result));
    assert_ne!(
        state.transcript.message_revisions[job.message_index],
        revisions_before[job.message_index]
    );
    assert_eq!(
        state.transcript.message_revisions[job.message_index + 1],
        revisions_before[job.message_index + 1]
    );
    assert!(Arc::ptr_eq(
        state
            .edit_highlights
            .applied()
            .get(&state.transcript.message_revisions[job.message_index])
            .map(|highlight| &highlight.styles)
            .expect("applied styles"),
        &expected_styles
    ));
    assert_eq!(
        state.edit_highlights.applied()[&state.transcript.message_revisions[job.message_index]]
            .tool_id,
        job.tool_id
    );
    assert_eq!(
        state.edit_highlights.applied()[&state.transcript.message_revisions[job.message_index]]
            .display_path,
        job.display_path
    );
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_some()
    );
    assert_eq!(state.pending_edit_highlight_count(), 0);
}

#[test]
fn refinement_rebuilds_only_matching_message_then_steady_and_scroll_build_nothing() {
    use std::cell::RefCell;

    let (_directory, mut state, job) = state_with_submitted_edit_job();
    state.push_message(ChatMessage::System("stable".to_string()));
    let stable_index = job.message_index + 1;
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    let built_indices = RefCell::new(Vec::new());

    {
        let messages = &state.transcript.messages;
        let revisions = &state.transcript.message_revisions;
        let highlights = state.edit_highlights.applied();
        let cache = &mut state.transcript.render_cache;
        cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                let refined = AppState::refined_diff_styles_for_message(
                    revisions, highlights, index, message,
                );
                crate::ui::build_lines_for_message(
                    message,
                    theme,
                    width,
                    tick,
                    force_expand,
                    refined,
                )
            },
        );
    }
    assert_eq!(
        *built_indices.borrow(),
        vec![job.message_index, stable_index]
    );
    let revisions_before = state.transcript.message_revisions.clone();
    built_indices.borrow_mut().clear();

    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    {
        let messages = &state.transcript.messages;
        let revisions = &state.transcript.message_revisions;
        let highlights = state.edit_highlights.applied();
        let cache = &mut state.transcript.render_cache;
        cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                let refined = AppState::refined_diff_styles_for_message(
                    revisions, highlights, index, message,
                );
                crate::ui::build_lines_for_message(
                    message,
                    theme,
                    width,
                    tick,
                    force_expand,
                    refined,
                )
            },
        );
    }

    assert_eq!(*built_indices.borrow(), vec![job.message_index]);
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 1);
    assert_eq!(
        state.transcript.message_revisions[stable_index],
        revisions_before[stable_index]
    );
    let viewport = state
        .transcript
        .render_cache
        .viewport(0, usize::MAX, usize::MAX);
    let inserted = inserted_source_line(&viewport.lines, "value = 2");
    assert!(
        inserted
            .spans
            .iter()
            .any(|span| { span.style.fg == Some(ratatui::style::Color::Magenta) })
    );

    built_indices.borrow_mut().clear();
    {
        let messages = &state.transcript.messages;
        let revisions = &state.transcript.message_revisions;
        let highlights = state.edit_highlights.applied();
        let cache = &mut state.transcript.render_cache;
        cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                let refined = AppState::refined_diff_styles_for_message(
                    revisions, highlights, index, message,
                );
                crate::ui::build_lines_for_message(
                    message,
                    theme,
                    width,
                    tick,
                    force_expand,
                    refined,
                )
            },
        );
    }
    assert!(built_indices.borrow().is_empty());
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 0);

    let _ = state.transcript.render_cache.viewport(0, 0, 1);
    let _ = state
        .transcript
        .render_cache
        .viewport(0, usize::MAX, usize::MAX);
    assert!(built_indices.borrow().is_empty());
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 0);
}

#[test]
fn real_worker_result_becomes_exact_message_styles_and_warms_rendering() {
    const SCOPED_DIFF: &str = "\
--- a/item.py
+++ b/item.py
@@ -3,2 +3,2 @@
     \"\"\"
-    field = 0
+    field = 1
";
    let directory = tempfile::tempdir().expect("scoped edit workspace");
    std::fs::write(
        directory.path().join("item.py"),
        "\
class Item:
    \"\"\"Summary.
    \"\"\"
    field = 1
",
    )
    .expect("post-edit Python file");
    let mut state = state();
    state.configure_syntax_highlighting(
        directory.path().to_path_buf(),
        crate::syntax_highlight::SyntaxTheme::OneHalfDark,
        crate::terminal_capabilities::TerminalColorLevel::TrueColor,
    );
    submit_live_edit(&mut state, "scoped-edit", "item.py", SCOPED_DIFF);
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    let cold = crate::ui::build_lines_for_message(
        &state.transcript.messages[0],
        &theme,
        80,
        0,
        false,
        None,
    );

    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if state.poll_edit_highlight_results() {
            break;
        }
        assert!(
            state.edit_highlight_needs_tick(),
            "worker stopped pending without applying a result"
        );
        assert!(
            Instant::now() < deadline,
            "worker did not return before the bounded deadline"
        );
        std::thread::yield_now();
    }

    let refined = state
        .refined_diff_styles(0, "scoped-edit")
        .expect("exact message refinement");
    assert!(refined.contains_key(&3));
    assert!(refined.contains_key(&4));
    let warm = crate::ui::build_lines_for_message(
        &state.transcript.messages[0],
        &theme,
        80,
        0,
        false,
        Some(refined),
    );
    let cold_field = inserted_source_line(&cold, "    field = 1");
    let warm_field = inserted_source_line(&warm, "    field = 1");

    assert_ne!(warm_field.spans[1..], cold_field.spans[1..]);
    assert_eq!(
        normalized_source_spans(&warm_field.spans[1..]),
        normalized_source_spans(&refined[&4])
    );
    assert!(!state.edit_highlight_needs_tick());
}

#[test]
fn failed_result_finishes_pending_without_touching_or_noise() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    let revisions_before = state.transcript.message_revisions.clone();
    let messages_before = state.transcript.messages.len();

    assert!(!state.apply_edit_highlight_result(
        crate::edit_highlight_worker::EditHighlightResult {
            job: job.clone(),
            outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
        }
    ));

    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
    assert_eq!(state.transcript.message_revisions, revisions_before);
    assert_eq!(state.transcript.messages.len(), messages_before);
}

#[test]
fn stale_edit_highlight_identity_is_rejected_without_touching_message() {
    type Mutation = Box<dyn Fn(&mut AppState, &mut crate::edit_highlight_worker::EditHighlightJob)>;
    let mutations: Vec<Mutation> = vec![
        Box::new(|state, job| {
            state.touch_message(job.message_index);
        }),
        Box::new(|_, job| job.job_id += 1),
        Box::new(|_, job| job.message_index += 1),
        Box::new(|_, job| job.tool_id = "other-tool".to_string()),
        Box::new(|_, job| job.absolute_path = PathBuf::from("/other/item.py")),
        Box::new(|_, job| job.display_path = "src/other.py".to_string()),
        Box::new(|state, _| {
            state.set_syntax_theme_for_test(crate::syntax_highlight::SyntaxTheme::OneHalfLight);
        }),
        Box::new(|_, job| {
            job.syntax_theme_revision = crate::terminal_capabilities::syntax_style_revision(
                crate::syntax_highlight::SyntaxTheme::OneHalfLight,
                job.syntax_color_level,
            );
        }),
        Box::new(|state, job| {
            let ChatMessage::ToolCall { diff, .. } =
                &mut state.transcript.messages[job.message_index]
            else {
                unreachable!();
            };
            *diff = Some(EDIT_DIFF.replace("value = 2", "value = 3"));
        }),
        Box::new(|state, job| {
            let ChatMessage::ToolCall { target, .. } =
                &mut state.transcript.messages[job.message_index]
            else {
                unreachable!();
            };
            *target = Some("src/other.py".to_string());
        }),
    ];

    for mutate in mutations {
        let (_directory, mut state, mut job) = state_with_submitted_edit_job();
        mutate(&mut state, &mut job);
        let revisions_before = state.transcript.message_revisions.clone();

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
        assert_eq!(state.transcript.message_revisions, revisions_before);
    }
}

#[test]
fn stale_edit_highlight_is_rejected_when_only_syntax_color_level_changes() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    let syntax_theme = state.syntax_theme_for_test();
    state
        .set_syntax_color_level_for_test(crate::terminal_capabilities::TerminalColorLevel::Ansi256);
    let revisions_before = state.transcript.message_revisions.clone();

    assert_eq!(state.syntax_theme_for_test(), syntax_theme);
    assert_ne!(state.syntax_color_level_for_test(), job.syntax_color_level);
    assert_ne!(
        crate::terminal_capabilities::syntax_style_revision(
            state.syntax_theme_for_test(),
            state.syntax_color_level_for_test(),
        ),
        job.syntax_theme_revision
    );
    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
    assert_eq!(state.transcript.message_revisions, revisions_before);
}

#[test]
fn ready_result_rejects_current_failed_status_without_touching() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    let ChatMessage::ToolCall { status, .. } = &mut state.transcript.messages[job.message_index]
    else {
        unreachable!();
    };
    *status = "failed".to_string();
    let revisions = state.transcript.message_revisions.clone();

    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_eq!(state.transcript.message_revisions, revisions);
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
}

#[test]
fn ready_result_rejects_current_row_tool_id_and_finishes_pending() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    let ChatMessage::ToolCall { id, .. } = &mut state.transcript.messages[job.message_index] else {
        unreachable!();
    };
    *id = "different-current-id".to_string();
    let revisions = state.transcript.message_revisions.clone();

    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert_eq!(state.transcript.message_revisions, revisions);
    assert!(state.edit_highlights.applied().is_empty());
}

#[test]
fn ready_result_rejects_current_diff_destination_mismatch() {
    let (directory, mut state, job) = state_with_submitted_edit_job();
    std::fs::write(directory.path().join("src/other.py"), "value = 2\n")
        .expect("other post-edit file");
    let ChatMessage::ToolCall { diff, .. } = &mut state.transcript.messages[job.message_index]
    else {
        unreachable!();
    };
    *diff = Some(EDIT_DIFF.replace("src/item.py", "src/other.py"));
    let revisions = state.transcript.message_revisions.clone();

    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_eq!(state.transcript.message_revisions, revisions);
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn ready_result_rejects_retargeted_symlink_on_apply_path() {
    use std::os::unix::fs::symlink;

    let (directory, mut state, job) = real_alias_edit_state();
    std::fs::write(directory.path().join("src/other.py"), "value = 2\n")
        .expect("other post-edit file");
    let alias = directory.path().join("src/alias.py");
    assert_eq!(job.display_path, "src/alias.py");
    std::fs::remove_file(&alias).expect("remove initial alias");
    symlink(directory.path().join("src/other.py"), &alias).expect("retarget alias");
    let revisions = state.transcript.message_revisions.clone();

    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert_eq!(state.transcript.message_revisions, revisions);
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
}

#[test]
fn stale_result_does_not_remove_newer_pending_job_for_same_tool() {
    let (_directory, mut state, stale_job) = state_with_submitted_edit_job();
    submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
    let newer_job = state
        .pending_edit_highlight_job("edit-1")
        .expect("newer pending job");
    assert_ne!(stale_job.job_id, newer_job.job_id);

    assert!(!state.apply_edit_highlight_result(ready_result(stale_job)));
    assert_eq!(
        state
            .pending_edit_highlight_job("edit-1")
            .expect("newer job preserved")
            .job_id,
        newer_job.job_id
    );
}

#[test]
fn touch_mutate_and_replace_cancel_only_their_exact_pending_message() {
    for action in ["touch", "mutate", "replace"] {
        let (_directory, mut state) = configured_edit_state();
        submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
        let job_a = state
            .pending_edit_highlight_job("edit-a")
            .expect("pending A");
        submit_live_edit(&mut state, "edit-b", "src/item.py", EDIT_DIFF);
        let job_b = state
            .pending_edit_highlight_job("edit-b")
            .expect("pending B");
        assert_eq!(state.pending_edit_highlight_count(), 2);

        match action {
            "touch" => {
                assert!(state.touch_message(job_a.message_index));
            }
            "mutate" => {
                state
                    .mutate_message(job_a.message_index, |message| {
                        let ChatMessage::ToolCall { expanded, .. } = message else {
                            unreachable!();
                        };
                        *expanded = true;
                    })
                    .expect("mutate A");
            }
            "replace" => {
                let replacement = state.transcript.messages[job_a.message_index].clone();
                assert!(state.replace_message(job_a.message_index, replacement));
            }
            _ => unreachable!(),
        }

        assert!(
            state.pending_edit_highlight_job("edit-a").is_none(),
            "{action}"
        );
        assert_eq!(
            state
                .pending_edit_highlight_job("edit-b")
                .expect("unrelated B remains")
                .job_id,
            job_b.job_id,
            "{action}"
        );
        assert_eq!(state.pending_edit_highlight_count(), 1, "{action}");
        assert!(state.edit_highlight_needs_tick(), "{action}");
        assert!(state.apply_edit_highlight_result(ready_result(job_b.clone())));
        assert!(
            state
                .refined_diff_styles(job_b.message_index, &job_b.tool_id)
                .is_some()
        );
    }
}

#[test]
fn replacing_non_tool_message_keeps_unrelated_edit_pending() {
    let (_directory, mut state) = configured_edit_state();
    state.push_message(ChatMessage::Reasoning("old".to_string()));
    submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
    let job = state
        .pending_edit_highlight_job("edit-a")
        .expect("pending edit");

    assert!(state.replace_message(0, ChatMessage::Reasoning("new".to_string())));

    assert_eq!(
        state
            .pending_edit_highlight_job("edit-a")
            .expect("edit pending survives")
            .job_id,
        job.job_id
    );
    assert!(state.edit_highlight_needs_tick());
    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_some()
    );
}

#[test]
fn disconnected_worker_is_abandoned_silently_and_next_edit_respawns() {
    fn disconnected(
        _runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
    ) -> crate::edit_highlight_worker::DrainResults {
        crate::edit_highlight_worker::DrainResults {
            results: Vec::new(),
            disconnected: true,
        }
    }

    let (_directory, mut state, _job) = state_with_submitted_edit_job();
    let revisions_before = state.transcript.message_revisions.clone();
    let messages_before = state.transcript.messages.len();
    state.set_edit_highlight_drain_for_test(Some(disconnected));

    assert!(!state.poll_edit_highlight_results());
    assert!(!state.edit_highlight_runtime_started());
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert_eq!(state.transcript.message_revisions, revisions_before);
    assert_eq!(state.transcript.messages.len(), messages_before);

    state.set_edit_highlight_drain_for_test(None);
    submit_live_edit(&mut state, "edit-2", "src/item.py", EDIT_DIFF);
    assert!(state.edit_highlight_runtime_started());
    assert_eq!(state.pending_edit_highlight_count(), 1);
}

#[test]
fn tool_touch_mutate_and_replace_remove_applied_map_before_revision_change() {
    for action in ["touch", "mutate", "replace"] {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        let revision = state.transcript.message_revisions[job.message_index];

        match action {
            "touch" => {
                state.touch_message(job.message_index);
            }
            "mutate" => {
                state.mutate_message(job.message_index, |message| {
                    let ChatMessage::ToolCall { expanded, .. } = message else {
                        unreachable!();
                    };
                    *expanded = true;
                });
            }
            "replace" => {
                let replacement = state.transcript.messages[job.message_index].clone();
                state.replace_message(job.message_index, replacement);
            }
            _ => unreachable!(),
        }

        assert!(state.transcript.message_revisions[job.message_index] > revision);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
    }
}

#[test]
fn message_lifecycle_prunes_applied_maps_and_pending_jobs() {
    for action in ["clear", "replace", "truncate", "retain"] {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        submit_live_edit(&mut state, "edit-2", "src/item.py", EDIT_DIFF);
        assert_eq!(state.pending_edit_highlight_count(), 1);

        match action {
            "clear" => state.clear_messages(),
            "replace" => state.replace_messages([ChatMessage::System("new".to_string())]),
            "truncate" => state.truncate_messages(job.message_index),
            "retain" => {
                state.retain_messages(|message| !matches!(message, ChatMessage::ToolCall { .. }))
            }
            _ => unreachable!(),
        }

        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
        assert_eq!(state.pending_edit_highlight_count(), 0);
    }
}

#[test]
fn retained_reindexing_clears_all_pending_jobs_conservatively() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    state
        .transcript
        .messages
        .insert(0, ChatMessage::System("remove".to_string()));
    state.reconcile_message_tracking();
    assert_eq!(state.pending_edit_highlight_count(), 1);

    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
    );

    assert_eq!(state.pending_edit_highlight_count(), 0);
    let revisions = state.transcript.message_revisions.clone();
    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert_eq!(state.transcript.message_revisions, revisions);
    assert!(
        state
            .refined_diff_styles(job.message_index, &job.tool_id)
            .is_none()
    );
}

#[test]
fn removed_message_result_and_reused_identity_never_inherit_styles() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    state.clear_messages();
    state.push_message(ChatMessage::ToolCall {
        id: job.tool_id.clone(),
        name: "edit".to_string(),
        target: Some(job.display_path.clone()),
        status: "completed".to_string(),
        output: None,
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
        expanded: false,
    });

    assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
    assert!(state.refined_diff_styles(0, &job.tool_id).is_none());
}

#[test]
fn direct_push_with_reused_tool_id_does_not_inherit_applied_styles() {
    let (_directory, mut state, job) = state_with_submitted_edit_job();
    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));

    state.push_message(ChatMessage::ToolCall {
        id: job.tool_id.clone(),
        name: "edit".to_string(),
        target: Some(job.display_path.clone()),
        status: "running".to_string(),
        output: None,
        diff: None,
        kind: None,
        expanded: false,
    });

    assert!(state.refined_diff_styles(0, &job.tool_id).is_none());
    assert!(state.refined_diff_styles(1, &job.tool_id).is_none());
}

#[test]
fn duplicate_tool_id_map_is_bound_to_exact_message_revision() {
    let (_directory, mut state) = configured_edit_state();
    state.push_message(ChatMessage::ToolCall {
        id: "duplicate".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
        status: "completed".to_string(),
        output: Some("older".to_string()),
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
        expanded: false,
    });
    submit_live_edit(&mut state, "duplicate", "src/item.py", EDIT_DIFF);
    let job = state
        .pending_edit_highlight_job("duplicate")
        .expect("newer duplicate job");
    assert_eq!(job.message_index, 1);

    assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
    assert!(
        AppState::refined_diff_styles_for_message(
            &state.transcript.message_revisions,
            state.edit_highlights.applied(),
            0,
            &state.transcript.messages[0],
        )
        .is_none()
    );
    assert!(
        AppState::refined_diff_styles_for_message(
            &state.transcript.message_revisions,
            state.edit_highlights.applied(),
            1,
            &state.transcript.messages[1],
        )
        .is_some()
    );

    state.truncate_messages(1);

    assert!(state.refined_diff_styles(0, "duplicate").is_none());
    assert!(state.edit_highlights.applied().is_empty());
}

#[test]
fn partial_prune_keeps_unrelated_applied_revision() {
    let (_directory, mut state) = configured_edit_state();
    submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
    let job_a = state
        .pending_edit_highlight_job("edit-a")
        .expect("pending A");
    submit_live_edit(&mut state, "edit-b", "src/item.py", EDIT_DIFF);
    let job_b = state
        .pending_edit_highlight_job("edit-b")
        .expect("pending B");
    assert!(state.apply_edit_highlight_result(ready_result(job_a)));
    assert!(state.apply_edit_highlight_result(ready_result(job_b)));
    assert!(state.refined_diff_styles(0, "edit-a").is_some());
    assert!(state.refined_diff_styles(1, "edit-b").is_some());

    state.truncate_messages(1);

    assert!(state.refined_diff_styles(0, "edit-a").is_some());
    assert_eq!(state.edit_highlights.applied().len(), 1);
}

#[test]
fn reused_tool_id_live_submission_applies_only_to_new_row() {
    let (_directory, mut state, first_job) = state_with_submitted_edit_job();
    assert!(state.apply_edit_highlight_result(ready_result(first_job)));
    assert!(state.refined_diff_styles(0, "edit-1").is_some());

    submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
    let new_job = state
        .pending_edit_highlight_job("edit-1")
        .expect("new reused job");
    assert_eq!(new_job.message_index, 1);
    assert!(state.apply_edit_highlight_result(ready_result(new_job)));

    assert!(state.refined_diff_styles(0, "edit-1").is_none());
    assert!(state.refined_diff_styles(1, "edit-1").is_some());
}

#[test]
fn disconnected_job_sender_drops_runtime_without_noise_or_extra_revision() {
    fn disconnected_runtime() -> std::io::Result<crate::edit_highlight_worker::EditHighlightRuntime>
    {
        Ok(crate::edit_highlight_worker::EditHighlightRuntime::disconnected_for_test())
    }

    let (_directory, mut state) = configured_edit_state();
    state.set_edit_highlight_runtime_factory_for_test(disconnected_runtime);
    state.update(TuiEvent::ToolRequested {
        id: "send-failure".to_string(),
        name: "edit".to_string(),
        target: Some("src/item.py".to_string()),
    });
    let revision_before_completion = state.transcript.message_revisions[0];

    state.update(TuiEvent::ToolCompleted {
        id: "send-failure".to_string(),
        name: "edit".to_string(),
        status: "completed".to_string(),
        output: "edited src/item.py".to_string(),
        diff: Some(EDIT_DIFF.to_string()),
        kind: None,
    });

    assert_eq!(state.transcript.messages.len(), 1);
    assert_eq!(
        state.transcript.message_revisions[0],
        revision_before_completion.saturating_add(1)
    );
    assert!(matches!(
        &state.transcript.messages[0],
        ChatMessage::ToolCall {
            status,
            output: Some(output),
            ..
        } if status == "completed" && output == "edited src/item.py"
    ));
    assert_eq!(state.pending_edit_highlight_count(), 0);
    assert_eq!(state.successful_edit_highlight_submit_count(), 0);
    assert!(!state.edit_highlight_runtime_started());
}
