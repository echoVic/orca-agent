//! Viewport, selection, and mouse-frame state for the fullscreen TUI.

use std::time::Instant;

use ratatui::layout::Rect;

use crate::selection::TranscriptSelection;

/// Transient status-line feedback after a mouse copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyNotice {
    pub chars: usize,
    pub at: Instant,
    /// Too large for OSC 52; only the local helper received the text.
    pub local_only: bool,
}

pub(crate) struct ViewportState {
    pub(crate) scroll_offset: usize,
    pub(crate) auto_scroll: bool,
    pub(crate) total_lines: usize,
    pub(crate) visible_height: usize,
    pub(crate) selection: Option<TranscriptSelection>,
    pub(crate) transcript_area: Option<Rect>,
    pub(crate) viewport_base_row: usize,
    pub(crate) pending_clipboard_copy: Option<String>,
    pub(crate) last_left_click: Option<(Instant, u16, u16, u8)>,
    pub(crate) copy_notice: Option<CopyNotice>,
    pub(crate) drag_edge_scroll: Option<(i8, u16)>,
    pub(crate) jump_to_bottom_area: Option<Rect>,
    pub(crate) frame_area: Option<Rect>,
    pub(crate) input_area: Option<Rect>,
    pub(crate) search_area: Option<Rect>,
    pub(crate) composer_mouse_selecting: bool,
    pub(crate) unseen_messages: usize,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            auto_scroll: true,
            ..Self::default_fields()
        }
    }
}

impl ViewportState {
    const fn default_fields() -> Self {
        Self {
            scroll_offset: 0,
            auto_scroll: false,
            total_lines: 0,
            visible_height: 0,
            selection: None,
            transcript_area: None,
            viewport_base_row: 0,
            pending_clipboard_copy: None,
            last_left_click: None,
            copy_notice: None,
            drag_edge_scroll: None,
            jump_to_bottom_area: None,
            frame_area: None,
            input_area: None,
            search_area: None,
            composer_mouse_selecting: false,
            unseen_messages: 0,
        }
    }
}

#[cfg(test)]
#[path = "viewport_state_tests.rs"]
mod tests;
