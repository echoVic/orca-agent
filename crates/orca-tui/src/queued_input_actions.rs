use crossbeam_channel as mpsc;
use crossterm::event::{Event, KeyCode, KeyEvent};
use orca_core::config::RunConfig;
use std::sync::{Arc, Mutex};
use tui_textarea::TextArea;

use crate::commands;
use crate::composer_input_actions::{
    apply_composer_key_input, handle_composer_editor_shortcut, insert_composer_newline,
};
use crate::composer_textarea::{make_textarea, make_textarea_with_text, textarea_text};
use crate::mention_menu_actions::handle_mention_menu_key;
use crate::queued_input::QueuedUserMessage;
use crate::running_actions::handle_running_shortcut;
use crate::shortcuts::{RunningShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::slash_command_actions::{SlashOutcome, handle_slash_command};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, PanelMode, UserAction};
use crate::vim::VimState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueuedDispatch {
    Started,
    None,
    Blocked,
    Failed,
}

pub(crate) fn enqueue_composer_follow_up(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    let Some(message) = QueuedUserMessage::from_composer(
        textarea_text(textarea),
        state.pending_pastes.clone(),
        state.mention_bindings.clone(),
    ) else {
        return false;
    };

    if state.enqueue_user_message(message).is_err() {
        state.report_queued_input_error("queued follow-up limit reached".to_string());
        return false;
    }

    state.slash_menu = None;
    state.mention.clear_projection();
    state.pending_pastes.clear();
    state.mention_bindings.clear();
    state.atomic_skill_tokens.clear();
    state.reset_history_navigation();
    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea(vim_state, theme);
    true
}

pub(crate) fn restore_latest_queued_message(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if state.panel_mode != PanelMode::Conversation
        || !matches!(state.status, AppStatus::Idle | AppStatus::Running)
        || state.transcript_search.open
        || state.show_shortcuts
        || state.slash_menu.is_some()
        || state.mention.phase.is_some()
    {
        return false;
    }
    let Some(composer) = state
        .pop_latest_queued_message()
        .map(QueuedUserMessage::into_composer_state)
    else {
        return false;
    };

    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea_with_text(&composer.visible_text, vim_state, theme);
    state.mention_bindings = composer.mention_bindings;
    state.atomic_skill_tokens.clear();
    state.pending_pastes = composer.pending_pastes;
    state.reset_history_navigation();
    true
}

pub(crate) fn dispatch_next_queued_user_message(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> QueuedDispatch {
    if state.queued_submission_in_flight() {
        return QueuedDispatch::Blocked;
    }
    let Some(action) = state.begin_next_queued_message() else {
        return QueuedDispatch::None;
    };

    match action_tx.try_send(action) {
        Ok(()) => {
            state.commit_queued_submission_admission();
            QueuedDispatch::Started
        }
        Err(mpsc::TrySendError::Full(_)) => {
            state.fail_queued_submission_dispatch("follow-up action queue is full".to_string());
            QueuedDispatch::Failed
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            state.fail_queued_submission_dispatch("follow-up action channel is closed".to_string());
            QueuedDispatch::Failed
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_running_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if state.show_shortcuts {
        return true;
    }
    if state.panel_mode == PanelMode::Conversation
        && (!state.mention.candidates.is_empty()
            || (state.mention.phase.is_some() && key.code == KeyCode::Esc))
        && handle_mention_menu_key(ev, key, state, textarea, vim_state, theme)
    {
        vim_state.cancel_pending_command();
        return true;
    }

    if state.panel_mode == PanelMode::Conversation
        && handle_composer_editor_shortcut(ev, key, state, config, textarea, vim_state, theme)
    {
        return true;
    }

    if let Some(ShortcutAction::Running(shortcut)) =
        resolve_shortcut(ShortcutContext::Running, *key)
    {
        vim_state.cancel_pending_command();
        match shortcut {
            RunningShortcut::SubmitQueued => {
                if state.panel_mode != PanelMode::Conversation {
                    return false;
                }
                let text = textarea_text(textarea).trim().to_string();
                if text.starts_with('/') {
                    if let Some(outcome) =
                        handle_slash_command(&text, config, shared_config, state, action_tx)
                    {
                        reset_after_running_slash(state, textarea, vim_state, theme, outcome);
                    } else {
                        state.push_message(crate::types::ChatMessage::Error(
                            commands::invalid_slash_command_message(&text),
                        ));
                        reset_after_running_slash(
                            state,
                            textarea,
                            vim_state,
                            theme,
                            SlashOutcome::Continue,
                        );
                    }
                    return true;
                }
                enqueue_composer_follow_up(state, textarea, vim_state, theme);
            }
            RunningShortcut::Newline => {
                if state.panel_mode != PanelMode::Conversation {
                    return false;
                }
                insert_composer_newline(textarea, state);
            }
            RunningShortcut::EditLatestQueued => {
                if state.panel_mode != PanelMode::Conversation {
                    return false;
                }
                restore_latest_queued_message(state, textarea, vim_state, theme);
            }
            shortcut => handle_running_shortcut(shortcut, state, action_tx),
        }
        return true;
    }

    if state.panel_mode == PanelMode::Conversation {
        return apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
    }
    false
}

fn reset_after_running_slash(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    outcome: SlashOutcome,
) {
    state.slash_menu = None;
    state.mention.clear_projection();
    state.pending_pastes.clear();
    state.mention_bindings.clear();
    state.atomic_skill_tokens.clear();
    state.reset_history_navigation();
    vim_state.reset_insert(textarea, theme);
    *textarea = match outcome {
        SlashOutcome::Continue => make_textarea(vim_state, theme),
        SlashOutcome::Prefill(value) => make_textarea_with_text(&value, vim_state, theme),
    };
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use orca_runtime::mentions::MentionBindings;

    use super::*;
    use crate::composer_textarea::{
        make_textarea_with_text, textarea_cursor_byte_index, textarea_text,
    };
    use crate::queued_input::QueuedUserMessage;
    use crate::types::{AppState, AppStatus, ChatMessage};
    use crate::vim::VimState;

    fn state() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        state
    }

    fn queued(text: &str) -> QueuedUserMessage {
        QueuedUserMessage::from_composer(text.to_string(), Vec::new(), MentionBindings::default())
            .unwrap()
    }

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    #[test]
    fn running_ctrl_u_clears_non_empty_follow_up_before_half_page_scroll() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = state();
        state.total_lines = 100;
        state.visible_height = 20;
        state.scroll_offset = 40;
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("follow up", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL);

        assert!(handle_running_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "");
        assert_eq!(state.scroll_offset, 40);
    }

    #[test]
    fn running_ctrl_b_moves_cursor_in_draft_before_background_action() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("follow up", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Char('b'), crossterm::event::KeyModifiers::CONTROL);

        assert!(handle_running_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "follow up");
        assert_eq!(textarea_cursor_byte_index(&textarea), "follow u".len());
        assert_eq!(state.status, AppStatus::Running);
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn enqueue_from_composer_clears_only_after_acceptance() {
        let mut state = state();
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("follow up", &vim, &theme);

        assert!(enqueue_composer_follow_up(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(state.queued_pending_visible_text().len(), 1);
        assert_eq!(textarea_text(&textarea), "");
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.pending_pastes.is_empty());
        assert!(state.mention_bindings.is_empty());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(_)))
        );
    }

    #[test]
    fn full_queue_keeps_composer_and_emits_no_transcript_error() {
        let mut state = state();
        for index in 0..crate::channels::USER_ACTION_CAPACITY {
            state
                .enqueue_user_message(queued(&format!("{index}")))
                .unwrap();
        }
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("keep me", &vim, &theme);

        assert!(!enqueue_composer_follow_up(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "keep me");
        assert!(state.queued_input_error().is_some());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::Error(_)))
        );
    }

    #[test]
    fn restore_latest_replaces_draft_and_preserves_earlier_fifo_items() {
        let mut state = state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.enqueue_user_message(queued("latest")).unwrap();
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft", &vim, &theme);

        assert!(restore_latest_queued_message(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "latest");
        assert_eq!(state.queued_pending_visible_text().len(), 1);
        assert_eq!(
            state.queued_pending_visible_text().first().copied(),
            Some("first")
        );
        assert!(state.queued_input_error().is_none());
    }

    #[test]
    fn queued_dispatch_sends_one_fifo_item_nonblocking() {
        let (action_tx, action_rx) = mpsc::bounded(1);
        let mut state = state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.enqueue_user_message(queued("second")).unwrap();
        state.set_status(AppStatus::Idle);

        assert_eq!(
            dispatch_next_queued_user_message(&mut state, &action_tx),
            QueuedDispatch::Started
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitQueued { prompt, .. }) if prompt == "first"
        ));
        assert_eq!(state.queued_pending_visible_text(), vec!["second"]);
        assert!(state.queued_submission_in_flight());
        assert_eq!(
            state.input_history.last().map(String::as_str),
            Some("first")
        );
    }

    #[test]
    fn full_and_disconnected_action_channels_restore_queue_front() {
        for (disconnected, expected_error) in [
            (false, "follow-up action queue is full"),
            (true, "follow-up action channel is closed"),
        ] {
            let (action_tx, action_rx) = mpsc::bounded(1);
            if disconnected {
                drop(action_rx);
            } else {
                action_tx
                    .send(UserAction::Remember {
                        scope: crate::types::TuiMemoryScope::User,
                        note: "occupy".to_string(),
                    })
                    .unwrap();
            }
            let mut state = state();
            state.enqueue_user_message(queued("first")).unwrap();
            state.set_status(AppStatus::Idle);

            assert_eq!(
                dispatch_next_queued_user_message(&mut state, &action_tx),
                QueuedDispatch::Failed,
                "disconnected={disconnected}"
            );
            assert_eq!(state.status, AppStatus::Idle);
            assert_eq!(state.queued_pending_visible_text(), vec!["first"]);
            assert!(!state.queued_submission_in_flight());
            assert!(
                !state
                    .messages
                    .iter()
                    .any(|message| matches!(message, ChatMessage::User(text) if text == "first"))
            );
            assert!(!state.input_history.iter().any(|entry| entry == "first"));
            assert_eq!(state.queued_input_error(), Some(expected_error));
            assert!(state.queued_autosend_enabled());
        }
    }

    #[test]
    fn non_conversation_running_enter_never_queues_composer_text() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        state.panel_mode = PanelMode::Workflows;
        let mut config = crate::test_support::test_run_config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("hidden draft", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);

        assert!(!handle_running_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert!(state.queued_pending_visible_text().is_empty());
        assert_eq!(textarea_text(&textarea), "hidden draft");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn workflows_command_opens_panel_while_running() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        let mut config = crate::test_support::test_run_config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/workflows", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);

        assert!(handle_running_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert!(state.queued_pending_visible_text().is_empty());
        assert_eq!(textarea_text(&textarea), "");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn unknown_slash_command_is_not_queued_while_running() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        let mut config = crate::test_support::test_run_config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/does-not-exist", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);

        assert!(handle_running_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert!(state.queued_pending_visible_text().is_empty());
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message)) if message.contains("unknown slash command")
        ));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn shortcuts_overlay_blocks_running_composer_edit_and_submit() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state();
        state.show_shortcuts = true;
        let mut config = crate::test_support::test_run_config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft", &vim, &theme);

        for key in [
            KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        ] {
            assert!(handle_running_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared_config,
                &action_tx,
                &mut textarea,
                &mut vim,
                &theme,
            ));
        }

        assert_eq!(textarea_text(&textarea), "draft");
        assert!(state.queued_pending_visible_text().is_empty());
        assert!(action_rx.try_recv().is_err());
    }
}
