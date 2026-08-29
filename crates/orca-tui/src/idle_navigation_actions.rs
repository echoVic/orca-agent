use crossbeam_channel as mpsc;

use crossterm::event::{Event, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;

use crate::composer_input_actions::apply_composer_key_input;
use crate::composer_textarea::textarea_text;
use crate::protocol::UserAction;
use crate::shortcuts::IdleShortcut;
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_navigation_shortcut(
    shortcut: IdleShortcut,
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    action_tx: &mpsc::Sender<UserAction>,
) {
    match shortcut {
        IdleShortcut::ScrollUp => {
            if textarea.lines().len() > 1 {
                textarea.input(Input::from(ev.clone()));
            } else {
                state.scroll_up(1);
            }
        }
        IdleShortcut::ScrollDown => {
            if textarea.lines().len() > 1 {
                textarea.input(Input::from(ev.clone()));
            } else {
                state.scroll_down(1);
            }
        }
        IdleShortcut::PageUp => {
            let page = state.viewport.visible_height.saturating_sub(2);
            state.scroll_up(page);
        }
        IdleShortcut::PageDown => {
            let page = state.viewport.visible_height.saturating_sub(2);
            state.scroll_down(page);
        }
        IdleShortcut::HalfPageUp => {
            let page = state.viewport.visible_height / 2;
            state.scroll_up(page);
        }
        IdleShortcut::HalfPageDown => {
            let page = state.viewport.visible_height / 2;
            state.scroll_down(page);
        }
        IdleShortcut::Backtrack => {
            let _ = action_tx.send(UserAction::Backtrack);
        }
        IdleShortcut::ExpandToolOutput => {
            if textarea_text(textarea).trim().is_empty() && state.toggle_latest_tool_output() {
                vim_state.cancel_pending_command();
                state.scroll_to_bottom();
            } else {
                apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
            }
        }
        IdleShortcut::Submit
        | IdleShortcut::Newline
        | IdleShortcut::EditLatestQueued
        | IdleShortcut::HistoryPrevious
        | IdleShortcut::HistoryNext => {}
    }
}
