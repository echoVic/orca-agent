//! Keys and mode menu of the `/rewind` dialog. The dialog's three steps are
//! the checkpoint list, the action menu of a checkpoint, and the confirm page
//! of that action's dry-run. The runtime does the listing, the dry-run and
//! the apply; this module only turns keys into `UserAction`s and keeps the
//! selection inside the dialog. The panel is drawn in `rewind_panel`.

use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orca_core::rewind::{RewindCheckpointView, RewindMode};

use crate::protocol::UserAction;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, RewindPhase, RewindState};

/// How many checkpoints the list shows at once, each two rows high. The
/// panel clamps this to the room the terminal gives it.
pub(crate) const REWIND_VISIBLE_CHECKPOINTS: usize = 9;

/// The entries of the action menu: every [`RewindMode`] in its canonical
/// order, then the cancel entry.
pub(crate) const REWIND_MODE_CHOICES: usize = RewindMode::ALL.len() + 1;

/// The mode at `index` of the action menu: `None` for the trailing cancel
/// entry.
pub(crate) fn rewind_mode_at(index: usize) -> Option<RewindMode> {
    RewindMode::ALL.get(index).copied()
}

/// Where `mode` sits in the action menu, for going back to the menu from the
/// confirm page.
pub(crate) fn rewind_mode_index(mode: RewindMode) -> usize {
    RewindMode::ALL
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(REWIND_MODE_CHOICES - 1)
}

/// What the menu calls `mode`.
pub(crate) fn rewind_mode_label(mode: RewindMode) -> &'static str {
    match mode {
        RewindMode::Both => "Restore code and conversation",
        RewindMode::Conversation => "Rewind conversation only",
        RewindMode::Code => "Restore code only",
        RewindMode::SummarizeFrom => "Summarize from here",
        RewindMode::SummarizeUpTo => "Summarize up to here",
    }
}

/// One line saying what choosing `mode` does.
pub(crate) fn rewind_mode_description(mode: RewindMode) -> &'static str {
    match mode {
        RewindMode::Both => {
            "restore files to before this message and cut the conversation back to it"
        }
        RewindMode::Conversation => "cut the conversation back to this message; files stay",
        RewindMode::Code => "restore files to before this message; the conversation stays",
        RewindMode::SummarizeFrom => "replace this message and everything after with a summary",
        RewindMode::SummarizeUpTo => "replace everything before this message with a summary",
    }
}

/// Why `mode` cannot be chosen for `view`, when it cannot: the runtime's
/// unavailable reasons, plus the summarize modes, which this version does
/// not implement yet (`orca_core::rewind` documents the reservation).
pub(crate) fn rewind_mode_unavailable_reason(
    view: &RewindCheckpointView,
    mode: RewindMode,
) -> Option<String> {
    if mode.is_summarize() {
        return Some("not implemented yet".to_string());
    }
    let mut reasons = Vec::new();
    if mode.includes_files() && !view.files_available {
        if view.snapshot_evicted {
            reasons.push("its file snapshot was evicted".to_string());
        } else if !view.has_snapshot {
            reasons.push("it has no file snapshot".to_string());
        } else {
            reasons.push("its files cannot be restored".to_string());
        }
    }
    if mode.includes_conversation() && !view.conversation_available {
        reasons.push("the conversation it belongs to is no longer available".to_string());
    }
    if !reasons.is_empty() {
        // The runtime's own reasons explain the flags; keep both, deduped.
        for reason in &view.unavailable_reasons {
            if !reasons.contains(reason) {
                reasons.push(reason.clone());
            }
        }
        return Some(reasons.join("; "));
    }
    None
}

/// The window of the checkpoint list that shows `selected`, given the
/// previous window start.
pub(crate) fn rewind_list_window(
    selected: usize,
    scroll: usize,
    len: usize,
    visible: usize,
) -> usize {
    let visible = visible.max(1).min(len.max(1));
    let mut start = scroll.min(len.saturating_sub(1));
    if selected < start {
        start = selected;
    } else if selected >= start + visible {
        start = selected + 1 - visible;
    }
    start
}

pub(crate) fn handle_rewind_key(
    key: &KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    let Some(mut rewind) = state.rewind.take() else {
        return;
    };
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut next_phase = None;
    let mut action = None;
    let mut close = false;
    let mut notice = None;
    let mut submit = false;

    // Ctrl+C always closes the dialog without applying anything, whatever
    // the step.
    if control && key.code == KeyCode::Char('c') {
        state.rewind = None;
        return;
    }

    match &mut rewind.phase {
        RewindPhase::PickCheckpoint { selected, scroll } => {
            let len = rewind.checkpoints.len();
            let last = len.saturating_sub(1);
            let page = REWIND_VISIBLE_CHECKPOINTS;
            match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::Char('k') if plain => *selected = selected.saturating_sub(1),
                KeyCode::Char('p') if control => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') if plain => {
                    *selected = (*selected + 1).min(last)
                }
                KeyCode::Char('n') if control => *selected = (*selected + 1).min(last),
                KeyCode::PageUp => *selected = selected.saturating_sub(page),
                KeyCode::PageDown => *selected = (*selected + page).min(last),
                KeyCode::Home => *selected = 0,
                KeyCode::End => *selected = last,
                KeyCode::Char('g') if plain => *selected = 0,
                KeyCode::Char('G') if plain => *selected = last,
                KeyCode::Enter if !rewind.pending => {
                    if len > 0 {
                        next_phase = Some(RewindPhase::PickMode {
                            checkpoint: *selected,
                            selected: 0,
                            show_preview: false,
                        });
                    }
                }
                _ => {}
            }
            *scroll = rewind_list_window(*selected, *scroll, len, page);
        }
        RewindPhase::PickMode {
            checkpoint,
            selected,
            show_preview,
        } => {
            let len = REWIND_MODE_CHOICES;
            let last = len - 1;
            match key.code {
                KeyCode::Esc => {
                    next_phase = Some(RewindPhase::PickCheckpoint {
                        selected: *checkpoint,
                        scroll: rewind_list_window(
                            *checkpoint,
                            0,
                            rewind.checkpoints.len(),
                            REWIND_VISIBLE_CHECKPOINTS,
                        ),
                    })
                }
                KeyCode::Tab => *show_preview = !*show_preview,
                KeyCode::Up | KeyCode::Char('k') if plain => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') if plain => {
                    *selected = (*selected + 1).min(last)
                }
                KeyCode::Home => *selected = 0,
                KeyCode::End => *selected = last,
                KeyCode::Enter if !rewind.pending => {
                    let chosen = *selected;
                    if chosen >= REWIND_MODE_CHOICES - 1 {
                        close = true;
                    } else {
                        let mode = RewindMode::ALL[chosen];
                        match rewind.checkpoints.get(*checkpoint) {
                            None => close = true,
                            Some(view) => match rewind_mode_unavailable_reason(view, mode) {
                                Some(reason) => {
                                    notice = Some(format!(
                                        "cannot rewind to #{} with this mode: {reason}",
                                        view.ordinal + 1
                                    ));
                                }
                                None => {
                                    action = Some(UserAction::RewindToCheckpoint {
                                        target: view.message_id.clone(),
                                        mode,
                                        dry_run: true,
                                    });
                                    submit = true;
                                }
                            },
                        }
                    }
                }
                KeyCode::Char(digit) if plain => {
                    if let Some(digit) = digit.to_digit(10) {
                        let index = digit as usize;
                        if (1..=REWIND_MODE_CHOICES).contains(&index) {
                            *selected = index - 1;
                        }
                    }
                }
                _ => {}
            }
        }
        RewindPhase::Confirm {
            checkpoint,
            mode,
            report: _,
            selected,
        } => match key.code {
            KeyCode::Esc | KeyCode::Char('n') if plain => {
                next_phase = Some(RewindPhase::PickMode {
                    checkpoint: *checkpoint,
                    selected: rewind_mode_index(*mode),
                    show_preview: true,
                });
            }
            KeyCode::Left | KeyCode::Up => *selected = 0,
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => *selected = 1,
            KeyCode::Char('y') if plain => {
                if let Some(view) = rewind.checkpoints.get(*checkpoint)
                    && !rewind.pending
                {
                    action = Some(UserAction::RewindToCheckpoint {
                        target: view.message_id.clone(),
                        mode: *mode,
                        dry_run: false,
                    });
                    submit = true;
                }
            }
            KeyCode::Enter if !rewind.pending => {
                let chosen = *selected;
                if chosen == 0
                    && let Some(view) = rewind.checkpoints.get(*checkpoint)
                {
                    action = Some(UserAction::RewindToCheckpoint {
                        target: view.message_id.clone(),
                        mode: *mode,
                        dry_run: false,
                    });
                    submit = true;
                } else {
                    next_phase = Some(RewindPhase::PickMode {
                        checkpoint: *checkpoint,
                        selected: rewind_mode_index(*mode),
                        show_preview: true,
                    });
                }
            }
            _ => {}
        },
    }

    if close {
        state.rewind = None;
        return;
    }
    if let Some(notice) = notice {
        rewind.error = Some(notice.clone());
        state.rewind = Some(rewind);
        state.push_message(ChatMessage::Error(notice));
        return;
    }
    if let Some(phase) = next_phase {
        rewind.phase = phase;
        rewind.error = None;
    }
    if submit {
        rewind.pending = true;
    }
    state.rewind = Some(rewind);
    if let Some(action) = action {
        let _ = action_tx.send(action);
    }
}

impl RewindState {
    /// A checkpoint's display number: the runtime counts from zero, people
    /// from one.
    pub(crate) fn display_ordinal(number: u32) -> u32 {
        number + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(ordinal: u32, files_available: bool) -> RewindCheckpointView {
        RewindCheckpointView {
            message_id: format!("item_{ordinal}"),
            ordinal,
            prompt_preview: format!("prompt {ordinal}"),
            recorded_at: None,
            files_changed: 2,
            insertions: 10,
            deletions: 3,
            has_snapshot: files_available,
            snapshot_evicted: false,
            conversation_available: true,
            files_available,
            unavailable_reasons: Vec::new(),
        }
    }

    #[test]
    fn the_mode_menu_lists_every_mode_then_cancel() {
        assert_eq!(REWIND_MODE_CHOICES, 6);
        for (index, mode) in RewindMode::ALL.iter().enumerate() {
            assert_eq!(rewind_mode_at(index), Some(*mode));
            assert_eq!(rewind_mode_index(*mode), index);
        }
        assert_eq!(rewind_mode_at(REWIND_MODE_CHOICES - 1), None);
    }

    #[test]
    fn file_modes_are_unavailable_without_a_snapshot() {
        let no_snapshot = checkpoint(0, false);
        assert!(rewind_mode_unavailable_reason(&no_snapshot, RewindMode::Both).is_some());
        assert!(rewind_mode_unavailable_reason(&no_snapshot, RewindMode::Code).is_some());
        assert_eq!(
            rewind_mode_unavailable_reason(&no_snapshot, RewindMode::Conversation),
            None
        );
        let with_snapshot = checkpoint(1, true);
        assert_eq!(
            rewind_mode_unavailable_reason(&with_snapshot, RewindMode::Both),
            None
        );
    }

    #[test]
    fn summarize_modes_are_marked_not_implemented() {
        let view = checkpoint(0, true);
        assert_eq!(
            rewind_mode_unavailable_reason(&view, RewindMode::SummarizeFrom).as_deref(),
            Some("not implemented yet")
        );
        assert_eq!(
            rewind_mode_unavailable_reason(&view, RewindMode::SummarizeUpTo).as_deref(),
            Some("not implemented yet")
        );
    }

    #[test]
    fn the_list_window_follows_the_selection() {
        assert_eq!(rewind_list_window(0, 0, 20, 9), 0);
        assert_eq!(rewind_list_window(8, 0, 20, 9), 0);
        assert_eq!(rewind_list_window(9, 0, 20, 9), 1);
        assert_eq!(rewind_list_window(15, 4, 20, 9), 7);
        assert_eq!(rewind_list_window(4, 9, 20, 9), 4);
        assert_eq!(rewind_list_window(0, 3, 20, 9), 0);
        assert_eq!(rewind_list_window(0, 0, 0, 9), 0);
    }
}
