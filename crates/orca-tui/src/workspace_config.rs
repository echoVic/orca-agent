//! Workspace-root and syntax-state configuration helpers. Extracted from
//! `app.rs` (TUI convergence slice 4).

use std::path::{Path, PathBuf};

use crate::transcript_state::ChatMessage;
use crate::types::AppState;
use orca_core::config::RunConfig;

pub(crate) fn mention_search_roots(config: &RunConfig, workspace_fallback: &Path) -> Vec<PathBuf> {
    config
        .runtime_workspace_roots
        .as_ref()
        .filter(|roots| !roots.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            vec![
                config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| workspace_fallback.into()),
            ]
        })
}

pub(crate) fn syntax_workspace_root(config: &RunConfig) -> PathBuf {
    config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

#[allow(dead_code, reason = "shared with the TUI binary target")]
pub(crate) fn configure_tui_syntax_state(
    state: &mut AppState,
    workspace_root: PathBuf,
    syntax_theme: crate::syntax_highlight::SyntaxTheme,
    syntax_color_level: crate::terminal_capabilities::TerminalColorLevel,
) {
    state.configure_syntax_highlighting(workspace_root, syntax_theme, syntax_color_level);
}

#[allow(dead_code, reason = "shared with the TUI binary target")]
pub(crate) fn configure_and_preload_tui_state(
    state: &mut AppState,
    workspace_root: PathBuf,
    syntax_theme: crate::syntax_highlight::SyntaxTheme,
    syntax_color_level: crate::terminal_capabilities::TerminalColorLevel,
    messages: impl IntoIterator<Item = ChatMessage>,
) {
    configure_tui_syntax_state(state, workspace_root, syntax_theme, syntax_color_level);
    for message in messages {
        state.push_message(message);
    }
}
