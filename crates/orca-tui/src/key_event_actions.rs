use std::io;

use crossbeam_channel as mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use orca_core::config::RunConfig;

use crate::approval_mode_actions::cycle_approval_mode;
use crate::composer_image_actions::{handle_image_paste_shortcut, handle_image_viewer_key};
use crate::composer_input_actions::composer_editor_shortcut_is_active;
use crate::global_actions::{GlobalShortcutFlow, handle_global_shortcut};
use crate::protocol::UserAction;
use crate::shortcuts::{GlobalShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::types::{AppState, AppStatus, PanelMode};
use crate::vim::VimState;

pub(crate) enum KeyEventFlow {
    Continue,
    Exit(i32),
    Unhandled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchKeyFlow {
    NotSearch,
    Handled,
}

pub(crate) fn handle_transcript_search_key(key: KeyEvent, state: &mut AppState) -> SearchKeyFlow {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !state.transcript.search.open
    {
        return SearchKeyFlow::NotSearch;
    }

    match key.code {
        KeyCode::Esc => state.close_transcript_search(),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.search_previous();
        }
        KeyCode::Enter => state.search_next(),
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                state.search_previous();
            } else {
                state.search_next();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.transcript.search.clear_query();
            state.refresh_transcript_search();
        }
        KeyCode::Backspace => {
            if state.transcript.search.backspace() {
                state.refresh_transcript_search();
            }
        }
        KeyCode::Left => state.transcript.search.move_left(),
        KeyCode::Right => state.transcript.search.move_right(),
        KeyCode::Home => state.transcript.search.move_home(),
        KeyCode::End => state.transcript.search.move_end(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            state.transcript.search.insert_char(character);
            state.refresh_transcript_search();
        }
        _ => {}
    }
    SearchKeyFlow::Handled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyModifiers};
    use std::sync::{Arc, Mutex};
    use tui_textarea::TextArea;

    use crate::protocol::{TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};
    use crate::selection::{SelectionPos, TranscriptSelection};
    use crate::status_key_actions::handle_status_key;
    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::transcript_view::TranscriptRenderContext;
    use crate::ui::build_lines_for_messages;
    use orca_core::cancel::OperationIdAllocator;
    use orca_runtime::history::SessionTranscript;

    /// Drives a plain Esc through the real two-stage router: preflight
    /// first, then (only when preflight returns `Unhandled`) the status
    /// stage — exactly what `RendererInputRouter::route` does for a key
    /// event, minus the mouse/paste/resize handling this module's tests
    /// don't need. Used to pin the precedence table documented on
    /// `handle_key_event_preflight`.
    #[allow(clippy::too_many_arguments)]
    fn press_esc(
        state: &mut AppState,
        config: &mut RunConfig,
        action_tx: &mpsc::Sender<UserAction>,
        textarea: &mut TextArea,
        vim: &mut VimState,
        theme: &Theme,
    ) {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let flow = handle_key_event_preflight(
            key,
            state,
            config,
            action_tx,
            vim,
            !textarea.is_empty(),
            || Ok(()),
        )
        .unwrap();
        if !matches!(flow, KeyEventFlow::Unhandled) {
            return;
        }
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let preloaded: Arc<Mutex<Option<SessionTranscript>>> = Arc::new(Mutex::new(None));
        handle_status_key(
            &Event::Key(key),
            &key,
            state,
            config,
            &shared_config,
            action_tx,
            &preloaded,
            textarea,
            vim,
            theme,
            None,
            || Ok(()),
        )
        .unwrap();
    }

    fn state_with_search_matches() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(ChatMessage::System {
            text: "alpha one".to_string(),
            expanded: false,
        });
        state.push_message(ChatMessage::System {
            text: "alpha two".to_string(),
            expanded: false,
        });
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let messages = &state.transcript.messages;
        let revisions = &state.transcript.message_revisions;
        state.transcript.render_cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false),
            |_, message, theme, width, tick, force_expand| {
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        state.refresh_transcript_search();
        state
    }

    #[test]
    fn active_search_keys_edit_close_and_navigate_without_fallthrough() {
        let mut state = state_with_search_matches();
        assert_eq!(
            handle_transcript_search_key(
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
                &mut state,
            ),
            SearchKeyFlow::Handled
        );
        assert_eq!(state.transcript.search.query(), "alphaz");
        handle_transcript_search_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &mut state);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.transcript.search.query(), "alphz");
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(state.transcript.search.query(), "");

        state.replace_transcript_search_query("alpha");
        state.refresh_transcript_search();
        let first = state.transcript.search.active_ordinal();
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_ne!(state.transcript.search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut state,
        );
        assert_eq!(state.transcript.search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_ne!(state.transcript.search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &mut state,
        );
        assert_eq!(state.transcript.search.active_ordinal(), first);

        handle_transcript_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(!state.transcript.search.open);
    }

    #[test]
    fn search_ctrl_g_precedes_running_interrupt_and_ctrl_c_stays_global() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.enter_running();
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event_preflight(
                ctrl_g,
                &mut state,
                &config,
                &action_tx,
                &mut vim,
                false,
                || Ok(()),
            )
            .unwrap(),
            KeyEventFlow::Continue
        ));
        assert!(action_rx.try_recv().is_err());

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key_event_preflight(
            ctrl_c,
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn global_and_search_preflight_clear_only_pending_vim_command_state() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(true);
        vim.seed_pending_count_for_test();
        vim.set_named_register_for_test(0, "saved");
        vim.set_repeat_for_test();

        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(!vim.has_pending_command_for_test());
        assert_eq!(vim.named_register_for_test(0), Some(("saved", false)));
        assert!(vim.has_repeat_for_test());
    }

    #[test]
    fn config_dialog_owns_non_cancel_global_shortcuts() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.config_dialog = Some(crate::types::ConfigDialog {
            selected: 0,
            model: state.model_name.clone(),
            reasoning_effort: state.reasoning_effort,
            approval_mode: state.approval_mode,
        });
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Unhandled));
        assert!(!state.transcript.search.open);
        assert!(state.config_dialog.is_some());
    }

    #[test]
    fn full_access_confirmation_owns_cancel_before_running_interrupt() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.enter_running();
        state.full_access_confirmation = Some(crate::types::FullAccessConfirmation {
            selected: 1,
            model: None,
            reasoning_effort: None,
        });
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Unhandled));
        assert!(state.full_access_confirmation.is_some());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn draft_editor_shortcuts_precede_conflicting_global_actions() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);

        let draft_flow = handle_key_event_preflight(
            ctrl_f,
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            true,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(draft_flow, KeyEventFlow::Unhandled));
        assert!(!state.transcript.search.open);

        let empty_flow = handle_key_event_preflight(
            ctrl_f,
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(empty_flow, KeyEventFlow::Continue));
        assert!(state.transcript.search.open);
    }

    #[test]
    fn release_and_unknown_search_keys_do_not_mutate_query() {
        let mut state = state_with_search_matches();
        let before = state.transcript.search.query().to_string();
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        };
        assert_eq!(
            handle_transcript_search_key(release, &mut state),
            SearchKeyFlow::NotSearch
        );
        assert_eq!(
            handle_transcript_search_key(
                KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
                &mut state,
            ),
            SearchKeyFlow::Handled
        );
        assert_eq!(state.transcript.search.query(), before);
    }

    #[test]
    fn escape_closes_the_agent_workspace_before_idle_input_handling() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.show_agents();
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn esc_collapses_the_dock_before_anything_else_handles_it() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        // The dock can be expanded while the full Agents panel is also open
        // (activity_lines renders it under both PanelMode::Conversation and
        // PanelMode::Agents). Esc must collapse the dock first and leave the
        // panel open, proving the dock's priority over the panel-close arm.
        state.show_agents();
        state.tasks_dock_expanded = true;
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert!(!state.tasks_dock_expanded, "the dock collapses first");
        assert_eq!(
            state.panel_mode,
            PanelMode::Agents,
            "the panel stays open for a second Esc"
        );
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn esc_closes_workflows_in_one_press_when_the_dock_is_toggled_but_covered() {
        // Reproduces the review finding: `/workflows` covers the dock
        // without clearing `tasks_dock_expanded` (activity_lines only
        // renders the dock under Conversation/Agents), so the flag alone
        // used to make the first Esc collapse an invisible dock and swallow
        // the keypress, leaving the Workflows panel open until a second
        // Esc. `tasks_dock_visible()` must resolve this in one press.
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.toggle_tasks_dock();
        state.show_workflows();
        assert!(
            state.tasks_dock_expanded,
            "the flag stays on; only visibility changes"
        );
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert_eq!(
            state.panel_mode,
            PanelMode::Conversation,
            "one Esc closes the Workflows panel; the covered dock must not swallow it"
        );
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn esc_collapses_a_visible_dock_under_plain_conversation() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.toggle_tasks_dock();
        assert_eq!(state.panel_mode, PanelMode::Conversation);
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let flow = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert!(!state.tasks_dock_expanded);
        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn escape_returns_from_agent_transcript_before_closing_workspace() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.show_agents();
        let request = crate::protocol::TaskTranscriptRequest {
            task_id: "child".to_string(),
            expected_revision: 4,
        };
        state.begin_task_transcript_request(request.clone());
        state.update(crate::protocol::TuiEvent::TaskTranscriptResult {
            request: request.clone(),
            result: crate::protocol::TaskTranscriptResult::unavailable("checkpoint is not ready"),
        });
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        let first = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(first, KeyEventFlow::Continue));
        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert!(state.task_transcript().is_none());

        state.update(crate::protocol::TuiEvent::TaskTranscriptResult {
            request,
            result: crate::protocol::TaskTranscriptResult::unavailable("late checkpoint"),
        });
        assert!(
            state.task_transcript().is_none(),
            "closing the transcript must revoke ownership of late replies"
        );

        let second = handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(second, KeyEventFlow::Continue));
        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(action_rx.try_recv().is_err());
    }

    fn background_task(
        id: &str,
        task_type: orca_core::task_types::TaskType,
        status: orca_core::task_types::TaskStatus,
    ) -> orca_core::task_types::BackgroundTaskSummary {
        orca_core::task_types::BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type,
            status,
            is_backgrounded: true,
            lifetime: orca_core::task_types::TaskLifetime::Task,
            description: id.to_string(),
            created_at_ms: 1,
            started_at_ms: Some(1),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: (status == orca_core::task_types::TaskStatus::ApprovalRequired)
                .then(|| orca_core::task_types::PendingToolCallSummary {
                    id: format!("{id}-call"),
                    name: "edit".to_string(),
                    action: orca_core::approval_types::ActionKind::Write,
                    target: Some("README.md".to_string()),
                    arguments: "{\"path\":\"README.md\"}".to_string(),
                }),
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
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: Some(2),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(3),
        }
    }

    fn press(
        key: KeyEvent,
        state: &mut AppState,
        action_tx: &mpsc::Sender<UserAction>,
        composer_has_text: bool,
    ) -> KeyEventFlow {
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);
        handle_key_event_preflight(
            key,
            state,
            &config,
            action_tx,
            &mut vim,
            composer_has_text,
            || Ok(()),
        )
        .unwrap()
    }

    #[test]
    fn enter_opens_a_backgrounded_sessions_pending_approval() {
        use orca_core::task_types::{TaskStatus, TaskType};
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        // Another task comes first, so the approval is not simply the
        // workflow panel's current selection.
        state.replace_workflow_tasks_for_test(vec![
            background_task("shell", TaskType::Shell, TaskStatus::Running),
            background_task(
                "deploy",
                TaskType::MainSession,
                TaskStatus::ApprovalRequired,
            ),
        ]);

        let flow = press(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &action_tx,
            false,
        );

        assert!(matches!(flow, KeyEventFlow::Continue));
        assert_eq!(state.status, AppStatus::WaitingApproval);
        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.background_task_id.as_deref(), Some("deploy"));
        assert_eq!(dialog.tool, "edit");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn enter_with_a_typed_message_sends_it_instead_of_opening_the_dock() {
        use orca_core::task_types::{TaskStatus, TaskType};
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        let mut child = background_task("child", TaskType::Subagent, TaskStatus::Running);
        child.publication_revision = Some(7);
        state.replace_workflow_tasks_for_test(vec![
            child,
            background_task(
                "deploy",
                TaskType::MainSession,
                TaskStatus::ApprovalRequired,
            ),
        ]);
        // A click on the agent left it selected in the dock.
        state.agent_dock_selected_task_id = Some("child".to_string());

        let flow = press(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &action_tx,
            true,
        );

        assert!(matches!(flow, KeyEventFlow::Unhandled));
        assert!(state.approval_dialog.is_none());
        assert_eq!(state.panel_mode, PanelMode::Conversation);
    }

    #[test]
    fn dock_navigation_opens_typed_child_transcript_and_escape_returns_to_agent_list() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.replace_workflow_tasks_for_test(vec![orca_core::task_types::BackgroundTaskSummary {
            id: "child".to_string(),
            parent_task_id: None,
            task_type: orca_core::task_types::TaskType::Subagent,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: true,
            lifetime: orca_core::task_types::TaskLifetime::Task,
            description: "inspect".to_string(),
            created_at_ms: 1,
            started_at_ms: Some(1),
            completed_at_ms: None,
            command: None,
            agent_type: Some("general".to_string()),
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
            subagent_current_activity: Some("read: src/lib.rs".to_string()),
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: Some(1),
            last_activity_at_ms: Some(2),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(7),
        }]);
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::ReadTaskTranscript(crate::protocol::TaskTranscriptRequest {
                task_id,
                expected_revision: 7,
            })) if task_id == "child"
        ));
        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert_eq!(
            state
                .task_transcript()
                .map(|view| view.request.task_id.as_str()),
            Some("child")
        );

        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert!(state.task_transcript().is_none());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn dock_enter_focuses_a_bound_live_child_without_opening_a_checkpoint() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.replace_workflow_tasks_for_test(vec![orca_core::task_types::BackgroundTaskSummary {
            id: "child".to_string(),
            parent_task_id: None,
            task_type: orca_core::task_types::TaskType::Subagent,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: true,
            lifetime: orca_core::task_types::TaskLifetime::Task,
            description: "inspect".to_string(),
            created_at_ms: 1,
            started_at_ms: Some(1),
            completed_at_ms: None,
            command: None,
            agent_type: Some("general".to_string()),
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
            subagent_current_activity: Some("read: src/lib.rs".to_string()),
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: Some("child-thread".to_string()),
            subagent_batch_id: Some("batch".to_string()),
            subagent_batch_size: Some(1),
            subagent_turn: Some(1),
            last_activity_at_ms: Some(2),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(9),
        }]);
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision: 9,
            }) if task_id == "child"
        ));
        assert!(state.task_transcript().is_none());
    }

    #[test]
    fn escape_from_live_child_emits_return_and_clears_focus_after_ack() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.close_transcript_search();
        state.update(crate::protocol::TuiEvent::ChildFocusChanged {
            task_id: Some("child".to_string()),
        });
        let config = test_run_config();
        let mut vim = crate::vim::VimState::new(false);

        handle_key_event_preflight(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &config,
            &action_tx,
            &mut vim,
            false,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::ReturnToParentThread)
        ));

        state.update(crate::protocol::TuiEvent::ChildFocusChanged { task_id: None });
        assert_eq!(
            state.conversation_target(),
            &orca_core::conversation::ConversationTarget::Main
        );
    }

    /// Carried finding: a tool/permission approval reaching a *focused*
    /// child (the child's own turn asked for one while the user is watching
    /// it — reachable via `HostedChildFocus`'s `child_interaction_capabilities`,
    /// which registers `ToolApproval`/`PermissionRequest` interactions on the
    /// active child attachment; see `hosted_child.rs`) used to be shadowed:
    /// the focused-child branch above claimed Esc before `handle_status_key`
    /// ever saw `AppStatus::WaitingApproval`, so denying was impossible
    /// without first losing the child focus. The approval is what is on
    /// screen asking for a decision, so it must win.
    #[test]
    fn esc_prefers_a_pending_approval_over_returning_to_a_focused_parent() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.update(crate::protocol::TuiEvent::ChildFocusChanged {
            task_id: Some("child".to_string()),
        });
        state.update(crate::protocol::TuiEvent::ApprovalNeeded {
            key: TuiInteractionKey::new(
                OperationIdAllocator::new().allocate(),
                "req-1",
                TuiInteractionKind::Approval,
            ),
            tool: "bash".to_string(),
            target: None,
            preview: None,
        });
        assert!(
            state.conversation_target().task_id().is_some(),
            "test setup: still focused on the child"
        );
        assert_eq!(
            state.status,
            AppStatus::WaitingApproval,
            "test setup: an approval is pending"
        );
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(
            matches!(
                action_rx.try_recv(),
                Ok(UserAction::RespondToInteraction {
                    response: TuiInteractionResponse::Approval(false),
                    ..
                })
            ),
            "the on-screen approval wins and Esc denies it"
        );
        assert!(
            action_rx.try_recv().is_err(),
            "ReturnToParentThread must not also fire"
        );
        assert_eq!(
            state.conversation_target().task_id(),
            Some("child"),
            "denying the approval does not itself navigate away"
        );
    }

    // The remaining table rows not already pinned elsewhere in this module
    // (the tasks dock, a task-transcript checkpoint, and the Workflows/
    // Agents panel close all have dedicated coverage above). Each case here
    // sets up its row's trigger plus the next lower-priority trigger and
    // asserts only the higher one fired.

    #[test]
    fn esc_precedence_help_overlay_closes_before_it_can_clear_a_selection() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.show_shortcuts = true;
        state.viewport.selection =
            Some(TranscriptSelection::begin(SelectionPos { row: 0, col: 0 }));
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(!state.show_shortcuts, "the help overlay closes");
        assert!(
            state.viewport.selection.is_some(),
            "the selection underneath is untouched by this Esc"
        );
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn esc_precedence_selection_clears_before_it_can_backtrack() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Idle;
        state.viewport.selection =
            Some(TranscriptSelection::begin(SelectionPos { row: 0, col: 0 }));
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(state.viewport.selection.is_none(), "the selection clears");
        assert!(
            action_rx.try_recv().is_err(),
            "Idle's empty-composer backtrack must not also fire"
        );
    }

    #[test]
    fn esc_precedence_denies_a_pending_approval() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.update(crate::protocol::TuiEvent::ApprovalNeeded {
            key: TuiInteractionKey::new(
                OperationIdAllocator::new().allocate(),
                "req-1",
                TuiInteractionKind::Approval,
            ),
            tool: "bash".to_string(),
            target: None,
            preview: None,
        });
        assert_eq!(state.status, AppStatus::WaitingApproval, "test setup");
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(
            matches!(
                action_rx.try_recv(),
                Ok(UserAction::RespondToInteraction {
                    response: TuiInteractionResponse::Approval(false),
                    ..
                })
            ),
            "Esc denies the pending approval"
        );
        assert!(state.approval_dialog.is_none());
    }

    #[test]
    fn esc_precedence_interrupts_a_running_turn() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Running;
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn esc_precedence_clears_a_non_empty_composer_through_the_full_router() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Idle;
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea =
            crate::composer_textarea::make_textarea_with_text("half-written prompt", &vim, &theme);

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(textarea.is_empty(), "the draft clears");
        assert!(action_rx.try_recv().is_err(), "and does not backtrack");
    }

    #[test]
    fn esc_precedence_backtracks_when_idle_and_the_composer_is_empty() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Idle;
        let mut config = test_run_config();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();

        press_esc(
            &mut state,
            &mut config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Backtrack)));
    }
}

/// # Esc precedence
///
/// Esc is resolved by a two-stage pipeline: this function (preflight) runs
/// first, and only when it returns [`KeyEventFlow::Unhandled`] does
/// `status_key_actions::handle_status_key` (status) get a turn. Whichever
/// branch matches first — in either stage — wins, consumes the keypress, and
/// is the *only* effect that runs; nothing lower ever also fires for the
/// same keypress. **A new Esc meaning is a new row in the stack below, not
/// a new branch dropped in wherever seems convenient.**
///
/// At the level the spec asks for, Esc has five cases:
///
///   1. something is open on top of the conversation -> close only the
///      topmost one
///   2. the turn is Running or Compacting             -> interrupt it
///   3. Idle, and the composer has a draft             -> clear the draft
///   4. Idle, and the composer is empty                -> backtrack
///   5. the session picker is open                     -> return to the
///      conversation
///
/// Case 1's "topmost" is itself an ordered stack spanning both stages. In
/// source order:
///
/// Preflight (this function):
///   1. the image viewer (`composer_image_actions::handle_image_viewer_key`)
///   2. full-access confirmation pending (deferred to status)
///   3. a focused child thread -> `ReturnToParentThread`, UNLESS a tool or
///      permission approval is pending (`AppStatus::WaitingApproval`) — the
///      approval is what is on screen asking for a decision, so it gets Esc
///      before navigation does. Without this carve-out the branch below
///      would claim Esc first and the approval's deny (case 14) would never
///      run; see `esc_prefers_a_pending_approval_over_returning_to_a_focused_parent`.
///   4. plan approval / config dialog / user-input dialog pending (each
///      deferred to status)
///   5. transcript search open
///   6. the shortcuts help overlay (`show_shortcuts`)
///   7. an active transcript selection
///   8. a task-transcript checkpoint view (Agents panel)
///   9. the tasks dock, if visible (`tasks_dock_visible()`)
///   10. the Workflows/Agents panel
///
/// Status (`handle_status_key`, only once preflight returns `Unhandled`):
///   11. setup / session-picker phases (case 5: returns to the conversation)
///   12. full-access confirmation
///   13. a pending approval (`WaitingApproval`) -> Esc denies (see case 3
///       above for why a focused child does not shadow this)
///   14. plan approval
///   15. the recovery prompt
///   16. the config dialog
///   17. the questionnaire (user-input dialog) -> backs out one step at a
///       time
///   18. the mention popup / slash menu (inside Idle handling)
///   19. Idle: case 3 (clear a non-empty draft) or case 4 (backtrack)
///   20. Running/Compacting: case 2 (interrupt)
pub(crate) fn handle_key_event_preflight<F>(
    key: KeyEvent,
    state: &mut AppState,
    _config: &RunConfig,
    action_tx: &mpsc::Sender<UserAction>,
    vim_state: &mut VimState,
    composer_has_text: bool,
    clear_terminal: F,
) -> io::Result<KeyEventFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(KeyEventFlow::Continue);
    }

    if handle_image_viewer_key(key, state) {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Continue);
    }

    if state.full_access_confirmation.is_some() {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Unhandled);
    }

    // A focused child owns Esc as the return-to-parent action, UNLESS a
    // tool/permission approval is pending (`AppStatus::WaitingApproval`):
    // that dialog is what is on screen asking for a decision, so it must
    // get Esc before navigation does (see the precedence table above).
    // Standing aside here lets the key fall through to `Unhandled`, where
    // `handle_status_key`'s `WaitingApproval` arm denies it. Otherwise,
    // handle it before the global cancel binding so returning does not
    // interrupt the child turn.
    if key.code == KeyCode::Esc
        && key.modifiers.is_empty()
        && state.status != AppStatus::WaitingApproval
        && state.conversation_target().task_id().is_some()
    {
        vim_state.cancel_pending_command();
        let _ = action_tx.send(UserAction::ReturnToParentThread);
        return Ok(KeyEventFlow::Continue);
    }

    if let Some(ShortcutAction::Global(GlobalShortcut::Cancel)) =
        resolve_shortcut(ShortcutContext::Global, key)
    {
        vim_state.cancel_pending_command();
        return match handle_global_shortcut(
            GlobalShortcut::Cancel,
            state,
            action_tx,
            clear_terminal,
        )? {
            GlobalShortcutFlow::Continue => Ok(KeyEventFlow::Continue),
            GlobalShortcutFlow::Exit(code) => Ok(KeyEventFlow::Exit(code)),
        };
    }

    if state.plan_approval_dialog.is_some() {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Unhandled);
    }

    if state.config_dialog.is_some() {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Unhandled);
    }

    if state.user_input_dialog.is_some() {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Unhandled);
    }

    if handle_transcript_search_key(key, state) == SearchKeyFlow::Handled {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Continue);
    }

    if handle_image_paste_shortcut(key, state, action_tx) {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Continue);
    }

    if let Some(ShortcutAction::Global(shortcut)) = resolve_shortcut(ShortcutContext::Global, key) {
        if composer_editor_shortcut_is_active(key, composer_has_text, vim_state) {
            return Ok(KeyEventFlow::Unhandled);
        }
        vim_state.cancel_pending_command();
        return match handle_global_shortcut(shortcut, state, action_tx, clear_terminal)? {
            GlobalShortcutFlow::Continue => Ok(KeyEventFlow::Continue),
            GlobalShortcutFlow::Exit(code) => Ok(KeyEventFlow::Exit(code)),
        };
    }

    if state.show_shortcuts && key.code == KeyCode::Esc {
        vim_state.cancel_pending_command();
        state.show_shortcuts = false;
        return Ok(KeyEventFlow::Continue);
    }

    // Esc dismisses an active mouse selection before any other Esc meaning
    // (cancel turn, close panel); a second Esc then does the usual thing.
    if key.code == KeyCode::Esc && state.viewport.selection.is_some() {
        vim_state.cancel_pending_command();
        state.invalidate_selection();
        return Ok(KeyEventFlow::Continue);
    }

    if !composer_has_text
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Up | KeyCode::Down)
        && !state.workflow_tasks().is_empty()
    {
        vim_state.cancel_pending_command();
        if key.code == KeyCode::Up {
            state.select_previous_agent_dock_task();
        } else {
            state.select_next_agent_dock_task();
        }
        return Ok(KeyEventFlow::Continue);
    }

    // Enter on an empty composer acts on the dock: it opens the agent picked
    // with Shift+Up/Down, or else the approval a background task waits on. A
    // typed message is always sent, whatever the dock last had selected.
    if state.panel_mode == PanelMode::Conversation
        && key.code == KeyCode::Enter
        && key.modifiers.is_empty()
        && !composer_has_text
    {
        vim_state.cancel_pending_command();
        if let Some(task_id) = state.selected_agent_dock_task().map(|task| task.id.clone()) {
            if !crate::agent_workspace_actions::open_agent_task(state, action_tx, &task_id) {
                return Ok(KeyEventFlow::Unhandled);
            }
            return Ok(KeyEventFlow::Continue);
        }
        if state.status == AppStatus::Idle && state.open_pending_background_approval_dialog() {
            return Ok(KeyEventFlow::Continue);
        }
        return Ok(KeyEventFlow::Unhandled);
    }

    if key.code == KeyCode::BackTab
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        vim_state.cancel_pending_command();
        cycle_approval_mode(state, action_tx);
        return Ok(KeyEventFlow::Continue);
    }

    if matches!(
        state.status,
        AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
    ) && state.panel_mode == PanelMode::Agents
        && state.task_transcript().is_some()
        && key.code == KeyCode::Esc
    {
        vim_state.cancel_pending_command();
        state.clear_task_transcript();
        return Ok(KeyEventFlow::Continue);
    }

    // The tasks dock is the more transient layer: collapse it on its own
    // before falling through to the full Workflows/Agents panel close below,
    // so a dock expanded on top of an open panel takes one Esc at a time.
    // `tasks_dock_visible()` (not the raw `tasks_dock_expanded` flag) is the
    // check: the dock can be toggled on and then covered by `/workflows`
    // without ever being cleared, and an Esc that lands on an invisible
    // dock must fall through to whatever is actually on screen instead of
    // silently consuming the keypress.
    if state.tasks_dock_visible() && key.code == KeyCode::Esc {
        vim_state.cancel_pending_command();
        state.collapse_tasks_dock();
        return Ok(KeyEventFlow::Continue);
    }

    if matches!(
        state.status,
        AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
    ) && matches!(state.panel_mode, PanelMode::Workflows | PanelMode::Agents)
        && key.code == KeyCode::Esc
    {
        vim_state.cancel_pending_command();
        state.show_conversation();
        return Ok(KeyEventFlow::Continue);
    }

    Ok(KeyEventFlow::Unhandled)
}
