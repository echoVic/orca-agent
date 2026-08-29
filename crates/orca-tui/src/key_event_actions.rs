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
    use crossterm::event::KeyModifiers;

    use crate::test_support::test_run_config;
    use crate::theme::Theme;
    use crate::transcript_state::ChatMessage;
    use crate::transcript_view::TranscriptRenderContext;
    use crate::ui::build_lines_for_messages;

    fn state_with_search_matches() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(ChatMessage::System("alpha one".to_string()));
        state.push_message(ChatMessage::System("alpha two".to_string()));
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
}

pub(crate) fn handle_key_event_preflight<F>(
    key: KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
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

    if key.code == KeyCode::BackTab
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        vim_state.cancel_pending_command();
        cycle_approval_mode(config, state, action_tx);
        return Ok(KeyEventFlow::Continue);
    }

    if state.status == AppStatus::Idle
        && state.panel_mode == PanelMode::Workflows
        && key.code == KeyCode::Esc
    {
        vim_state.cancel_pending_command();
        state.show_conversation();
        return Ok(KeyEventFlow::Continue);
    }

    Ok(KeyEventFlow::Unhandled)
}
