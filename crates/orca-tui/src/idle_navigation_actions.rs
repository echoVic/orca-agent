use crossbeam_channel as mpsc;

use crossterm::event::Event;
use tui_textarea::{Input, TextArea};

use crate::protocol::UserAction;
use crate::shortcuts::IdleShortcut;
use crate::types::AppState;

pub(crate) fn handle_idle_navigation_shortcut(
    shortcut: IdleShortcut,
    ev: &Event,
    state: &mut AppState,
    textarea: &mut TextArea,
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
            if state.toggle_latest_expandable() {
                state.scroll_to_bottom();
            }
        }
        IdleShortcut::ExpandAll => {
            if state.toggle_all_expandable() {
                state.scroll_to_bottom();
            }
        }
        IdleShortcut::Submit
        | IdleShortcut::Newline
        | IdleShortcut::EditLatestQueued
        | IdleShortcut::HistoryPrevious
        | IdleShortcut::HistoryNext => {}
    }
}
