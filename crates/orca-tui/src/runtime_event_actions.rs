use crossbeam_channel as mpsc;

use tui_textarea::TextArea;

use crate::bridge;
use crate::composer_textarea::make_textarea_with_text;
use crate::queued_input_actions::{QueuedDispatch, dispatch_next_queued_user_message};
use crate::terminal_presentation::{TerminalNotification, TerminalPresentation};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, TuiEvent, UserAction};
use crate::vim::VimState;
use crate::workflow_notifications::{
    drain_pending_workflow_notifications, is_workflow_notification_turn_boundary,
    queue_workflow_terminal_notification, remove_pending_workflow_notification_by_id,
    submit_pending_workflow_notification,
};

pub(crate) fn terminal_notification_for_event(
    event: &TuiEvent,
    state: &AppState,
) -> Option<TerminalNotification> {
    let message = match event {
        TuiEvent::ApprovalNeeded { tool, target, .. }
            if !state.approval_is_allowlisted(tool, target.as_deref()) =>
        {
            "Approval required"
        }
        TuiEvent::ApprovalNeeded { .. } => return None,
        TuiEvent::PermissionApprovalNeeded { .. } => "Permission approval required",
        TuiEvent::UserInputRequested { .. } => "Input required",
        TuiEvent::McpElicitationRequested { .. } => "MCP input required",
        TuiEvent::SessionCompleted { status } if status == "success" => "Task completed",
        TuiEvent::SessionCompleted { status } => {
            return Some(TerminalNotification::new(format!("Task {status}")));
        }
        TuiEvent::WorkflowNotification { status, .. } if status == "completed" => {
            "Workflow completed"
        }
        TuiEvent::WorkflowNotification { status, .. } => {
            return Some(TerminalNotification::new(format!("Workflow {status}")));
        }
        _ => return None,
    };
    Some(TerminalNotification::new(message))
}

pub(crate) fn handle_runtime_event(
    tui_event: TuiEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    presentation: &mut TerminalPresentation,
) {
    let initial_status = state.status;
    let terminal_notification = terminal_notification_for_event(&tui_event, state);
    if let TuiEvent::ApprovalNeeded {
        key, tool, target, ..
    } = &tui_event
        && state.approval_is_allowlisted(tool, target.as_deref())
    {
        vim_state.flush_pending_insert_escape(textarea);
        vim_state.cancel_pending_command();
        let _ = action_tx.send(UserAction::RespondToInteraction {
            key: key.clone(),
            response: crate::types::TuiInteractionResponse::Approval(true),
        });
        state.enter_running();
        return;
    }

    let queued_submission_rejected = match &tui_event {
        TuiEvent::SubmissionRejected {
            queued_id: Some(id),
            ..
        } => state.queued_submission_matches_id(*id),
        _ => false,
    };
    let new_session_started = matches!(&tui_event, TuiEvent::NewSessionStarted);
    let restored_prompt = match &tui_event {
        TuiEvent::Backtracked { prompt } => Some(prompt.clone()),
        TuiEvent::SubmissionRejected { prompt, .. } if !queued_submission_rejected => {
            Some(prompt.clone())
        }
        _ => None,
    };
    let workflow_notification_turn_boundary = is_workflow_notification_turn_boundary(&tui_event);
    let batch_queued_workflow_notification_id = queue_workflow_terminal_notification(
        &tui_event,
        pending_workflow_notifications,
        state.status == AppStatus::Running,
    );

    let previous_status = state.status;
    state.update(tui_event);
    if state.status != previous_status {
        vim_state.flush_pending_insert_escape(textarea);
        vim_state.cancel_pending_command();
    }
    if let Some(notification) = terminal_notification {
        presentation.enqueue(notification);
    }

    if let Some(id) = batch_queued_workflow_notification_id {
        remove_pending_workflow_notification_by_id(state, &id);
    }
    if queued_submission_rejected {
        if let Some(composer) = state.take_rejected_queued_composer_state() {
            vim_state.flush_pending_insert_escape(textarea);
            vim_state.reset_insert(textarea, theme);
            *textarea = make_textarea_with_text(&composer.visible_text, vim_state, theme);
            state.mention_bindings = composer.mention_bindings;
            state.pending_pastes = composer.pending_pastes;
            state.reset_history_navigation();
        }
    } else if let Some(prompt) = restored_prompt {
        vim_state.flush_pending_insert_escape(textarea);
        vim_state.reset_insert(textarea, theme);
        *textarea = make_textarea_with_text(&prompt, vim_state, theme);
    }
    if new_session_started {
        vim_state.flush_pending_insert_escape(textarea);
        vim_state.reset_insert(textarea, theme);
        *textarea = make_textarea_with_text("", vim_state, theme);
    }
    if state.plan_approval_dialog.is_none() {
        if workflow_notification_turn_boundary {
            drain_pending_workflow_notifications(state, pending_workflow_notifications);
            if dispatch_next_queued_user_message(state, action_tx) == QueuedDispatch::None
                && !state.queued_follow_up_pending_or_in_flight()
            {
                submit_pending_workflow_notification(state, action_tx, false);
            }
        } else if !state.queued_follow_up_pending_or_in_flight() {
            submit_pending_workflow_notification(state, action_tx, true);
        }
    }
    if state.status != initial_status {
        vim_state.flush_pending_insert_escape(textarea);
        vim_state.cancel_pending_command();
    }
    if state.auto_scroll {
        state.scroll_to_bottom();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::textarea_text;
    use crate::queued_input::QueuedUserMessage;
    use crate::queued_input_actions::{QueuedDispatch, dispatch_next_queued_user_message};
    use crate::terminal_presentation::TerminalNotification;
    use crate::types::{
        ChatMessage, PendingWorkflowNotification, TuiInteractionKey, TuiInteractionKind,
    };
    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::{ThemeName, VimInsertEscapeSequence};
    use orca_runtime::mentions::{MentionBinding, MentionBindings, MentionFileKind, MentionTarget};
    use orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode;
    use std::path::PathBuf;
    use std::time::Instant;
    use tui_textarea::CursorMove;

    fn vim_insert_input(character: char) -> tui_textarea::Input {
        tui_textarea::Input {
            key: tui_textarea::Key::Char(character),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(OperationIdAllocator::new().allocate(), id, kind)
    }

    fn notification_message(event: &TuiEvent, state: &AppState) -> Option<TerminalNotification> {
        terminal_notification_for_event(event, state)
    }

    fn test_presentation() -> TerminalPresentation {
        TerminalPresentation::new(
            false,
            crate::terminal_presentation::TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        )
    }

    fn queue_text(state: &mut AppState, text: &str) {
        state
            .enqueue_user_message(
                QueuedUserMessage::from_composer(
                    text.to_string(),
                    Vec::new(),
                    MentionBindings::default(),
                )
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn terminal_boundary_promotes_user_follow_up_before_workflow_notification() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, "first");
        state
            .pending_workflow_notifications
            .push_back(PendingWorkflowNotification {
                id: "workflow-1".to_string(),
                prompt: "internal workflow".to_string(),
            });
        state.enter_running();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: "success".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "first"
        ));
        assert_eq!(state.pending_workflow_notifications.len(), 1);
    }

    #[test]
    fn idle_workflow_notification_waits_behind_held_user_follow_up() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, "user first");
        state.suspend_queued_follow_up_autosend();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::WorkflowNotification {
                id: "workflow-1".to_string(),
                prompt: "internal workflow".to_string(),
                status: "completed".to_string(),
                summary: "done".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.queued_pending_visible_text().len(), 1);
        assert_eq!(state.pending_workflow_notifications.len(), 1);
    }

    #[test]
    fn terminal_workflow_notification_waits_behind_interrupted_user_follow_up() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, "user first");
        state.suspend_queued_follow_up_autosend();
        state
            .pending_workflow_notifications
            .push_back(PendingWorkflowNotification {
                id: "workflow-1".to_string(),
                prompt: "internal workflow".to_string(),
            });
        state.enter_running();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: "cancelled".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.queued_pending_visible_text().len(), 1);
        assert_eq!(state.pending_workflow_notifications.len(), 1);
    }

    #[test]
    fn every_terminal_status_promotes_one_follow_up() {
        for status in ["success", "failed", "verification_failed", "cancelled"] {
            let (action_tx, action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            queue_text(&mut state, status);
            state.enter_running();
            let pending = bridge::PendingWorkflowNotifications::new();
            let theme = Theme::named(ThemeName::Dark);
            let mut textarea = TextArea::default();
            let mut vim = VimState::new(false);
            let mut presentation = test_presentation();

            handle_runtime_event(
                TuiEvent::SessionCompleted {
                    status: status.to_string(),
                },
                &mut state,
                &action_tx,
                &pending,
                &mut textarea,
                &mut vim,
                &theme,
                &mut presentation,
            );
            assert!(matches!(
                action_rx.try_recv(),
                Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == status
            ));
        }
    }

    #[test]
    fn occupied_admission_fence_blocks_late_terminal() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, "first");
        queue_text(&mut state, "second");
        assert_eq!(
            dispatch_next_queued_user_message(&mut state, &action_tx),
            QueuedDispatch::Started
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "first"
        ));
        state.set_status(AppStatus::Idle);
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SessionCompleted {
                status: "backgrounded".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.queued_pending_visible_text(), vec!["second"]);
    }

    #[test]
    fn rejected_promoted_follow_up_restores_visible_paste_and_mentions() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/workspace".to_string(),
        );
        let visible = "review @item.rs [Pasted Content 1001 chars]";
        let mention_start = visible.find("@item.rs").unwrap();
        let pasted = "payload\n".repeat(150);
        state
            .enqueue_user_message(
                QueuedUserMessage::from_composer(
                    visible.to_string(),
                    vec![("[Pasted Content 1001 chars]".to_string(), pasted.clone())],
                    MentionBindings::from_bindings(
                        visible,
                        vec![MentionBinding {
                            start: mention_start,
                            end: mention_start + "@item.rs".len(),
                            visible: "@item.rs".to_string(),
                            target: MentionTarget::File {
                                root: PathBuf::from("/workspace"),
                                path: "item.rs".to_string(),
                                kind: MentionFileKind::File,
                            },
                        }],
                    ),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            dispatch_next_queued_user_message(&mut state, &action_tx),
            QueuedDispatch::Started
        );
        let prompt = match action_rx.try_recv().unwrap() {
            UserAction::SubmitQueued { prompt, .. } => prompt,
            other => panic!("unexpected action: {other:?}"),
        };
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SubmissionRejected {
                queued_id: state.queued_in_flight_id(),
                prompt,
                message: "rejected".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert_eq!(textarea_text(&textarea), visible);
        assert_eq!(state.pending_pastes[0].1, pasted);
        assert_eq!(state.mention_bindings.bindings().len(), 1);
        assert!(!state.queued_submission_in_flight());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(text) if text == visible))
        );
    }

    #[test]
    fn unrelated_submission_rejection_does_not_restore_queued_fence() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        queue_text(&mut state, "queued prompt");
        assert_eq!(
            dispatch_next_queued_user_message(&mut state, &action_tx),
            QueuedDispatch::Started
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. })
                if prompt == "queued prompt"
        ));
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = TextArea::default();
        let mut vim = VimState::new(false);
        let mut presentation = test_presentation();

        handle_runtime_event(
            TuiEvent::SubmissionRejected {
                queued_id: Some(u64::MAX),
                prompt: "other prompt".to_string(),
                message: "other rejection".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(state.queued_submission_in_flight());
        assert_eq!(textarea_text(&textarea), "other prompt");
    }

    #[test]
    fn submission_rejection_restores_prompt_to_composer() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(crate::types::ChatMessage::User(
            "review @gone.txt".to_string(),
        ));
        state.enter_running();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim_state = VimState::new(false);
        let mut textarea = TextArea::default();
        let mut presentation = TerminalPresentation::new(
            false,
            crate::terminal_presentation::TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: false,
            },
        );

        handle_runtime_event(
            TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt: "review @gone.txt".to_string(),
                message: "bound file is no longer available".to_string(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim_state,
            &theme,
            &mut presentation,
        );

        assert_eq!(textarea_text(&textarea), "review @gone.txt");
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn terminal_notification_for_event_matches_fixed_safe_matrix() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );

        let cases = [
            (
                TuiEvent::ApprovalNeeded {
                    key: interaction_key(TuiInteractionKind::Approval, "approval"),
                    tool: "secret-tool".to_string(),
                    target: Some("secret-target".to_string()),
                    preview: Some("secret-preview".to_string()),
                },
                "Approval required",
            ),
            (
                TuiEvent::PermissionApprovalNeeded {
                    key: interaction_key(TuiInteractionKind::Permission, "permission"),
                    tool: "secret-tool".to_string(),
                    target: Some("secret-target".to_string()),
                    preview: Some("secret-preview".to_string()),
                    permission_kind:
                        orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry,
                },
                "Permission approval required",
            ),
            (
                TuiEvent::UserInputRequested {
                    key: interaction_key(TuiInteractionKind::UserInput, "input"),
                    question: "secret-question".to_string(),
                    choices: vec!["secret-choice".to_string()],
                },
                "Input required",
            ),
            (
                TuiEvent::McpElicitationRequested {
                    key: interaction_key(TuiInteractionKind::McpElicitation, "mcp"),
                    server_name: "secret-server".to_string(),
                    mode: RuntimeMcpElicitationMode::Form,
                    message: "secret-message".to_string(),
                    url: Some("secret-url".to_string()),
                    requested_schema_json: Some("secret-schema".to_string()),
                },
                "MCP input required",
            ),
            (
                TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                },
                "Task completed",
            ),
            (
                TuiEvent::SessionCompleted {
                    status: "verification_failed".to_string(),
                },
                "Task verification_failed",
            ),
            (
                TuiEvent::WorkflowNotification {
                    id: "secret-id".to_string(),
                    prompt: "secret-prompt".to_string(),
                    status: "completed".to_string(),
                    summary: "secret-summary".to_string(),
                },
                "Workflow completed",
            ),
            (
                TuiEvent::WorkflowNotification {
                    id: "secret-id".to_string(),
                    prompt: "secret-prompt".to_string(),
                    status: "failed".to_string(),
                    summary: "secret-summary".to_string(),
                },
                "Workflow failed",
            ),
        ];

        for (event, expected) in cases {
            let notification = notification_message(&event, &state).expect("notification");
            assert_eq!(notification.message(), expected);
            for secret in [
                "secret-tool",
                "secret-target",
                "secret-preview",
                "secret-question",
                "secret-choice",
                "secret-server",
                "secret-message",
                "secret-url",
                "secret-schema",
                "secret-id",
                "secret-prompt",
                "secret-summary",
            ] {
                assert!(!notification.message().contains(secret));
            }
        }
        assert!(notification_message(&TuiEvent::Notice("ignored".to_string()), &state).is_none());
    }

    #[test]
    fn terminal_notification_for_event_skips_allowlisted_approval() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state
            .approval_allowlist
            .insert(AppState::approval_key_target("bash", "cargo test"));

        let event = TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval"),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        };

        assert!(terminal_notification_for_event(&event, &state).is_none());
    }

    #[test]
    fn handle_runtime_event_enqueues_only_when_presentation_is_unfocused() {
        for (focused, expected_pending) in [(true, 0), (false, 1)] {
            let (action_tx, _action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            state.enter_running();
            let pending = bridge::PendingWorkflowNotifications::new();
            let theme = Theme::named(ThemeName::Dark);
            let mut vim_state = VimState::new(false);
            let mut textarea = TextArea::default();
            let mut presentation = TerminalPresentation::new(
                true,
                crate::terminal_presentation::TerminalPresentationProfile {
                    osc9_supported: true,
                    tmux_passthrough: false,
                },
            );
            presentation.set_focused(focused);

            handle_runtime_event(
                TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                },
                &mut state,
                &action_tx,
                &pending,
                &mut textarea,
                &mut vim_state,
                &theme,
                &mut presentation,
            );

            assert_eq!(presentation.pending_len_for_test(), expected_pending);
        }
    }

    #[test]
    fn new_session_started_clears_composer_without_resetting_projection() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut presentation = test_presentation();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut textarea = make_textarea_with_text("stale composer", &VimState::new(false), &theme);
        let mut vim = VimState::new(false);

        handle_runtime_event(
            TuiEvent::NewSessionStarted,
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(textarea_text(&textarea).is_empty());
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn runtime_status_transitions_clear_pending_vim_commands_but_streaming_does_not() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut presentation = test_presentation();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut textarea = TextArea::from(["draft"]);
        let mut vim = VimState::new(true);
        vim.seed_pending_count_for_test();

        handle_runtime_event(
            TuiEvent::MessageDelta("streaming".to_string()),
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert!(vim.has_pending_command_for_test());

        handle_runtime_event(
            TuiEvent::UserInputRequested {
                key: interaction_key(TuiInteractionKind::UserInput, "input"),
                question: "question".to_string(),
                choices: Vec::new(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert!(!vim.has_pending_command_for_test());
        vim.handle(
            tui_textarea::Input {
                key: tui_textarea::Key::Char('i'),
                ctrl: false,
                alt: false,
                shift: false,
            },
            &mut textarea,
            &theme,
        );
        assert_eq!(vim.mode, crate::vim::VimMode::Insert);
    }

    #[test]
    fn runtime_status_transition_flushes_pending_insert_escape_before_new_owner() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut presentation = test_presentation();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let started = Instant::now();
        let mut vim =
            VimState::with_insert_escape(true, Some(VimInsertEscapeSequence::parse("jj").unwrap()));
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::from(["draft"]);
        textarea.move_cursor(CursorMove::End);
        vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

        handle_runtime_event(
            TuiEvent::UserInputRequested {
                key: interaction_key(TuiInteractionKind::UserInput, "input"),
                question: "question".to_string(),
                choices: Vec::new(),
            },
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert_eq!(textarea_text(&textarea), "draftj");
        assert!(!vim.has_pending_insert_escape_for_test());
        assert_eq!(state.status, AppStatus::WaitingUserInput);
    }

    #[test]
    fn idle_workflow_auto_submission_clears_pending_vim_command() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let pending = bridge::PendingWorkflowNotifications::new();
        let theme = Theme::named(ThemeName::Dark);
        let mut presentation = test_presentation();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state
            .pending_workflow_notifications
            .push_back(PendingWorkflowNotification {
                id: "workflow-1".to_string(),
                prompt: "internal workflow".to_string(),
            });
        let mut textarea = TextArea::from(["draft"]);
        let mut vim = VimState::new(true);
        vim.seed_pending_count_for_test();

        handle_runtime_event(
            TuiEvent::Notice("wake".to_string()),
            &mut state,
            &action_tx,
            &pending,
            &mut textarea,
            &mut vim,
            &theme,
            &mut presentation,
        );

        assert_eq!(state.status, AppStatus::Running);
        assert!(!vim.has_pending_command_for_test());
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "workflow-1"
        ));
    }
}
