//! Insert-escape flush orchestration: input-ownership preprocessing that
//! flushes a pending Vim insert-escape sequence before paste/submit/shortcut
//! routing. Extracted from `app.rs` (TUI convergence slice 1); the routing
//! enum lives beside the policy it serves.

use ratatui::crossterm::event::{Event, KeyEventKind};
use std::time::Instant;

use crate::composer_input_actions::refresh_input_menus;
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::{PendingInsertEscapeFlow, VimState};
use orca_core::config::RunConfig;
use tui_textarea::{Input, TextArea};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInsertEscapeRouting {
    Continue,
    Consumed,
}

fn refresh_after_insert_escape_flush(
    state: &mut AppState,
    config: &RunConfig,
    textarea: &TextArea<'_>,
) {
    state.reset_history_navigation();
    refresh_input_menus(textarea, state, config);
}

pub(crate) fn resolve_pending_insert_escape_before_routing(
    event: &Event,
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
    theme: &Theme,
) -> PendingInsertEscapeRouting {
    let Event::Key(key) = event else {
        return PendingInsertEscapeRouting::Continue;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PendingInsertEscapeRouting::Continue;
    }
    match vim_state.resolve_pending_insert_escape(&Input::from(event.clone()), now, textarea) {
        PendingInsertEscapeFlow::Consumed => {
            vim_state.configure_block(textarea, theme);
            PendingInsertEscapeRouting::Consumed
        }
        PendingInsertEscapeFlow::Flushed => {
            refresh_after_insert_escape_flush(state, config, textarea);
            PendingInsertEscapeRouting::Continue
        }
        PendingInsertEscapeFlow::NoPending => PendingInsertEscapeRouting::Continue,
    }
}

pub(crate) fn flush_pending_insert_escape_before_non_key(
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_pending_insert_escape(textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}

pub(crate) fn flush_expired_insert_escape(
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_expired_insert_escape(now, textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}
