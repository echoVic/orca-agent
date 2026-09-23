//! Reverse lookup from a rendered transcript row back to the message that
//! drew it, plus the per-frame hit areas a mouse click uses to toggle a
//! collapsible message open or closed.
//!
//! Mirrors `image_preview::ImageHitArea` / `AppState::image_hit_areas`: hit
//! areas are recomputed from scratch every frame in
//! `ui::render_live_messages`, never accumulated, so they always match what
//! was last drawn.

use ratatui::layout::Rect;

use crate::chrome::TAIL_ROW;
use crate::transcript_state::ChatMessage;

/// One clickable row of a collapsible message for the frame just drawn, and
/// the index of the message it toggles.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CollapsibleHitArea {
    pub(crate) rect: Rect,
    pub(crate) message_index: usize,
}

/// Whether `message` can be expanded/collapsed, and therefore whether it
/// gets an entry in `collapsible_hit_areas`. Kept in one place so future
/// collapsible kinds extend a single match arm — `types.rs`'s
/// `toggle_latest_expandable`/`toggle_expandable_at`/`toggle_all_expandable`
/// all call this too, so the `e` key, `Shift+E`, and a mouse click never
/// disagree about what counts as collapsible.
///
/// `ChatMessage::System` only counts past two rows at `width`, the width the
/// transcript draws at: a notice of one or two rows already renders in full,
/// so it must not gain a `└ +N lines` tail row or a hit area that would
/// toggle nothing visible. Rows, not lines — a single long line such as the
/// sandbox warning wraps into several.
pub(crate) fn is_collapsible(message: &ChatMessage, width: usize) -> bool {
    match message {
        ChatMessage::ToolCall { .. } | ChatMessage::Reasoning { .. } => true,
        ChatMessage::System { text, .. } => crate::ui::system_notice_rows(text, width) > 2,
        _ => false,
    }
}

/// The message whose rendered rows contain `row`, given the cache's per-message
/// row ranges and the viewport's first visible row.
///
/// `area_y` is the transcript area's screen-space top row and `row` is the
/// screen-space row to resolve (a raw mouse event row, for instance) — the
/// same coordinates `image_hit_areas` anchors its rects to in
/// `render_live_messages`. Returns `None` for a row above the area or past
/// the last message's rows.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn message_index_at_row(
    row_ranges: &[std::ops::Range<usize>],
    viewport_base_row: usize,
    area_y: u16,
    row: u16,
) -> Option<usize> {
    let offset = row.checked_sub(area_y)?;
    let absolute_row = viewport_base_row + usize::from(offset);
    row_ranges
        .iter()
        .position(|range| range.contains(&absolute_row))
}

/// Rebuilds the rows that toggle a collapsible message for the frame about
/// to be drawn: its header (the first non-blank row, since a message may
/// open with a blank separator) and its `└ ` tail row, each while on screen.
/// Every other row holds content — expanded output, a notice's text — and a
/// click there must start a text selection like anywhere else in the
/// transcript, not collapse what the user is trying to copy.
///
/// `row_range_for` gives a message's absolute rows and `row_text_for` an
/// absolute row's text, both as the render cache laid them out.
pub(crate) fn collapsible_hit_areas<'t>(
    messages: &[ChatMessage],
    row_range_for: impl Fn(usize) -> Option<std::ops::Range<usize>>,
    row_text_for: impl Fn(usize) -> Option<&'t str>,
    viewport_base_row: usize,
    visible_height: usize,
    area: Rect,
) -> Vec<CollapsibleHitArea> {
    let visible = viewport_base_row..viewport_base_row.saturating_add(visible_height);
    let mut areas = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        if !is_collapsible(message, usize::from(area.width)) {
            continue;
        }
        let Some(range) = row_range_for(message_index) else {
            continue;
        };
        let header = range
            .clone()
            .find(|&row| row_text_for(row).is_some_and(|text| !text.trim().is_empty()));
        let on_screen = range.start.max(visible.start)..range.end.min(visible.end);
        let tails = on_screen.filter(|&row| {
            Some(row) != header && row_text_for(row).is_some_and(|text| text.starts_with(TAIL_ROW))
        });
        for row in header.into_iter().chain(tails) {
            if visible.contains(&row) {
                areas.push(CollapsibleHitArea {
                    rect: Rect::new(area.x, area.y + (row - visible.start) as u16, area.width, 1),
                    message_index,
                });
            }
        }
    }
    areas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call() -> ChatMessage {
        ChatMessage::ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            target: Some("src/main.rs".into()),
            status: "completed".into(),
            output: Some("done".into()),
            diff: None,
            kind: None,
            expanded: false,
        }
    }

    #[test]
    fn maps_a_screen_row_back_to_the_message_that_drew_it() {
        // message 0 occupies rows 0..2, message 1 rows 2..7, message 2 rows 7..8
        let ranges = vec![0..2, 2..7, 7..8];
        // viewport starts at absolute row 2 and is drawn at screen y = 10
        assert_eq!(message_index_at_row(&ranges, 2, 10, 10), Some(1));
        assert_eq!(message_index_at_row(&ranges, 2, 10, 14), Some(1));
        assert_eq!(message_index_at_row(&ranges, 2, 10, 15), Some(2));
        assert_eq!(
            message_index_at_row(&ranges, 2, 10, 9),
            None,
            "above the area"
        );
        assert_eq!(
            message_index_at_row(&ranges, 2, 10, 99),
            None,
            "past the last row"
        );
    }

    #[test]
    fn a_collapsible_message_is_clickable_on_its_header_only_while_it_is_on_screen() {
        let messages = vec![
            ChatMessage::User("hi".into()),
            tool_call(),
            ChatMessage::Reasoning {
                text: "a\nb".into(),
                expanded: false,
            },
        ];
        let ranges = [0..1, 1..4, 4..5];
        let rows = [
            " ›  hi",
            "",
            "    ✓ read_file src/main.rs",
            "    │ done",
            "    ⋯ thinking · a",
        ];
        let areas = collapsible_hit_areas(
            &messages,
            |index| ranges.get(index).cloned(),
            |row| rows.get(row).copied(),
            0,
            5,
            Rect::new(0, 3, 80, 5),
        );
        let hits = areas
            .iter()
            .map(|area| (area.message_index, area.rect.y, area.rect.height))
            .collect::<Vec<_>>();
        assert_eq!(
            hits,
            [(1, 5, 1), (2, 7, 1)],
            "the user message is not collapsible, and the tool's blank separator \
             and output row are not its header"
        );

        let scrolled = collapsible_hit_areas(
            &messages,
            |index| ranges.get(index).cloned(),
            |row| rows.get(row).copied(),
            3,
            2,
            Rect::new(0, 3, 80, 2),
        );
        let hits = scrolled
            .iter()
            .map(|area| (area.message_index, area.rect.y))
            .collect::<Vec<_>>();
        assert_eq!(hits, [(2, 4)], "the tool's header has scrolled off screen");
    }

    #[test]
    fn a_system_notice_only_gets_a_hit_area_past_two_lines() {
        let messages = vec![
            ChatMessage::System {
                text: "one line".into(),
                expanded: false,
            },
            ChatMessage::System {
                text: "one\ntwo".into(),
                expanded: false,
            },
            ChatMessage::System {
                text: "one\ntwo\nthree".into(),
                expanded: false,
            },
        ];
        let ranges = [0..1, 1..2, 2..4];
        let rows = [
            "    ℹ one line",
            "    ℹ one",
            "    ℹ one",
            "    └ +2 lines · click or e to expand",
        ];
        let areas = collapsible_hit_areas(
            &messages,
            |index| ranges.get(index).cloned(),
            |row| rows.get(row).copied(),
            0,
            4,
            Rect::new(0, 0, 80, 4),
        );
        let hits = areas
            .iter()
            .map(|area| (area.message_index, area.rect.y))
            .collect::<Vec<_>>();
        assert_eq!(
            hits,
            [(2, 2), (2, 3)],
            "a one- or two-line notice has nothing to expand; a longer one \
             toggles from its header and its tail: {areas:?}"
        );
    }

    #[test]
    fn a_one_line_notice_is_collapsible_only_where_it_wraps_past_two_rows() {
        let one_line = ChatMessage::System {
            text: "word ".repeat(40),
            expanded: false,
        };
        assert!(is_collapsible(&one_line, 60), "five rows at 60 columns");
        assert!(!is_collapsible(&one_line, 400), "a single row at 400");

        let three_lines = ChatMessage::System {
            text: "one\ntwo\nthree".into(),
            expanded: false,
        };
        assert!(is_collapsible(&three_lines, 400), "three rows at any width");

        let rows = [
            "    ℹ word word word…",
            "    └ +4 lines · click or e to expand",
        ];
        let areas = collapsible_hit_areas(
            std::slice::from_ref(&one_line),
            |_| Some(0..2),
            |row| rows.get(row).copied(),
            0,
            4,
            Rect::new(0, 0, 60, 4),
        );
        assert_eq!(
            areas.len(),
            2,
            "at the drawn width it collapses, so its header and tail both toggle"
        );
    }
}
