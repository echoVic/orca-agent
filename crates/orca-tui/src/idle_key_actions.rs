use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_image_actions::handle_composer_image_preview_key;
use crate::composer_input_actions::{
    apply_composer_key_input, handle_composer_editor_shortcut, insert_composer_newline,
    recall_next_history, recall_previous_history,
};
use crate::idle_navigation_actions::handle_idle_navigation_shortcut;
use crate::idle_submit_actions::handle_idle_submit;
use crate::mention_menu_actions::handle_mention_menu_key;
use crate::queued_input_actions::restore_latest_queued_message;
use crate::shortcuts::{IdleShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::slash_menu_actions::handle_slash_menu_key;
use crate::theme::Theme;
use crate::types::{AppState, UserAction};
use crate::vim::VimState;
use crate::workflow_panel_actions::handle_workflows_panel_key;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) {
    if state.slash_menu.is_some()
        && handle_slash_menu_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
        )
    {
        vim_state.cancel_pending_command();
        return;
    }

    if (!state.mention.candidates.is_empty()
        || (state.mention.phase.is_some()
            && (key.code == KeyCode::Esc
                || state.mention.sigil == Some(orca_runtime::mentions::MentionSigil::Dollar))))
        && handle_mention_menu_key(ev, key, state, textarea, vim_state, theme)
    {
        vim_state.cancel_pending_command();
        return;
    }

    if handle_workflows_panel_key(key.code, state, action_tx) {
        vim_state.cancel_pending_command();
        return;
    }

    if handle_composer_editor_shortcut(ev, key, state, config, textarea, vim_state, theme) {
        return;
    }
    if handle_composer_image_preview_key(*key, state, textarea) {
        vim_state.cancel_pending_command();
        return;
    }

    match resolve_shortcut(ShortcutContext::Idle, *key) {
        Some(ShortcutAction::Idle(IdleShortcut::EditLatestQueued)) => {
            vim_state.cancel_pending_command();
            if state.status == crate::types::AppStatus::Idle {
                restore_latest_queued_message(state, action_tx);
            }
        }
        Some(ShortcutAction::Idle(IdleShortcut::Submit)) => {
            vim_state.cancel_pending_command();
            handle_idle_submit(
                textarea,
                vim_state,
                theme,
                state,
                config,
                shared_config,
                action_tx,
            );
        }
        Some(ShortcutAction::Idle(IdleShortcut::Newline)) => {
            vim_state.cancel_pending_command();
            insert_composer_newline(textarea, state);
        }
        Some(ShortcutAction::Idle(IdleShortcut::HistoryPrevious)) => {
            vim_state.cancel_pending_command();
            recall_previous_history(ev, key, state, textarea, vim_state, theme);
        }
        Some(ShortcutAction::Idle(IdleShortcut::HistoryNext)) => {
            vim_state.cancel_pending_command();
            recall_next_history(ev, key, state, textarea, vim_state, theme);
        }
        Some(ShortcutAction::Idle(
            shortcut @ (IdleShortcut::ScrollUp
            | IdleShortcut::ScrollDown
            | IdleShortcut::PageUp
            | IdleShortcut::PageDown
            | IdleShortcut::HalfPageUp
            | IdleShortcut::HalfPageDown
            | IdleShortcut::Backtrack
            | IdleShortcut::ExpandToolOutput),
        )) => {
            if shortcut == IdleShortcut::Backtrack && !textarea.is_empty() {
                return;
            }
            if shortcut != IdleShortcut::ExpandToolOutput {
                vim_state.cancel_pending_command();
            }
            handle_idle_navigation_shortcut(
                shortcut, ev, key, state, config, textarea, vim_state, theme, action_tx,
            );
        }
        Some(_) | None => {
            apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::test_support::test_run_config;
    use crate::types::TuiEvent;
    use crossterm::event::KeyModifiers;
    use orca_core::config::{ThemeName, VimInsertEscapeSequence};

    #[test]
    fn ctrl_u_clears_non_empty_composer_before_half_page_scroll() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.total_lines = 100;
        state.visible_height = 20;
        state.scroll_offset = 40;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft\nmessage", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "");
        assert_eq!(state.scroll_offset, 40);
    }

    #[test]
    fn ctrl_u_keeps_half_page_scroll_when_composer_is_empty() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.total_lines = 100;
        state.visible_height = 20;
        state.scroll_offset = 40;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::default();
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(textarea.is_empty());
        assert_eq!(state.scroll_offset, 30);
    }

    #[test]
    fn escape_does_not_backtrack_over_a_non_empty_draft() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "draft");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn vim_normal_escape_stays_in_editor_instead_of_backtracking() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        let mut textarea = make_textarea_with_text("draft", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "draft");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn empty_skill_picker_consumes_enter_without_submitting_dollar_prompt() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.mention.sigil = Some(orca_runtime::mentions::MentionSigil::Dollar);
        state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = TextArea::from(["$"]);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea.lines(), &["$".to_string()]);
        assert!(action_rx.try_recv().is_err());
        assert!(state.messages.is_empty());
    }

    #[test]
    fn nonempty_composer_keeps_vim_count_when_e_matches_expand_shortcut() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        let mut textarea = TextArea::from(["one two three"]);

        for code in [KeyCode::Char('2'), KeyCode::Char('e')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_idle_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea.cursor(), (0, 6));
        assert!(!vim.has_pending_command_for_test());
    }

    #[test]
    fn empty_multiline_composer_keeps_vim_prefix_when_expand_has_no_tool() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        let mut textarea = TextArea::from([" ", " ", " "]);

        for code in [KeyCode::Char('d'), KeyCode::Char('e')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_idle_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea.cursor(), (0, 0));
        assert!(!vim.has_pending_command_for_test());
    }

    #[test]
    fn configured_first_character_does_not_steal_consumed_idle_shortcut() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "grep".to_string(),
            target: None,
        });
        let mut config = test_run_config();
        config.vim_mode = true;
        config.vim_insert_escape = Some(VimInsertEscapeSequence::parse("ee").unwrap());
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::with_insert_escape(true, config.vim_insert_escape.clone());
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(textarea.is_empty());
        assert!(!vim.has_pending_insert_escape_for_test());
        let crate::types::ChatMessage::ToolCall { expanded, .. } = &state.messages[0] else {
            panic!("expected tool call");
        };
        assert!(*expanded);
    }
}
