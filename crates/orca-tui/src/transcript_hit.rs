//! Reverse lookup from a rendered transcript row back to the message that
//! drew it, plus the per-frame hit areas a mouse click uses to toggle a
//! collapsible message open or closed.
//!
//! Mirrors `image_preview::ImageHitArea` / `AppState::image_hit_areas`: hit
//! areas are recomputed from scratch every frame in
//! `ui::render_live_messages`, never accumulated, so they always match what
//! was last drawn.

use ratatui::layout::Rect;

use crate::transcript_state::ChatMessage;

/// A collapsible message's on-screen rect for the frame just drawn, and the
/// index of the message it belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CollapsibleHitArea {
    pub(crate) rect: Rect,
    pub(crate) message_index: usize,
}

/// Whether `message` can be expanded/collapsed, and therefore whether it
/// gets an entry in `collapsible_hit_areas`. Kept in one place so future
/// collapsible kinds (e.g. `ChatMessage::System`) extend a single match arm —
/// `types.rs`'s `toggle_latest_expandable`/`toggle_expandable_at`/
/// `toggle_all_expandable` all call this too, so the `e` key, `Shift+E`, and
/// a mouse click never disagree about what counts as collapsible.
pub(crate) fn is_collapsible(message: &ChatMessage) -> bool {
    matches!(
        message,
        ChatMessage::ToolCall { .. } | ChatMessage::Reasoning { .. }
    )
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

/// Rebuilds the hit areas collapsible messages occupy on screen for the
/// frame about to be drawn. Same shape as the `image_hit_areas` rebuild in
/// `render_live_messages`: walk the messages once, keep only the
/// collapsible ones, intersect each one's row range with the visible
/// window, and drop anything the intersection leaves empty.
pub(crate) fn collapsible_hit_areas(
    messages: &[ChatMessage],
    row_range_for: impl Fn(usize) -> Option<std::ops::Range<usize>>,
    viewport_base_row: usize,
    visible_height: usize,
    area: Rect,
) -> Vec<CollapsibleHitArea> {
    let visible_start = viewport_base_row;
    let visible_end = visible_start.saturating_add(visible_height);
    messages
        .iter()
        .enumerate()
        .filter_map(|(message_index, message)| {
            if !is_collapsible(message) {
                return None;
            }
            let range = row_range_for(message_index)?;
            let start = range.start.max(visible_start);
            let end = range.end.min(visible_end);
            (start < end).then(|| CollapsibleHitArea {
                rect: Rect::new(
                    area.x,
                    area.y + start.saturating_sub(visible_start) as u16,
                    area.width,
                    end.saturating_sub(start) as u16,
                ),
                message_index,
            })
        })
        .collect()
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
    fn only_collapsible_messages_get_a_hit_area_and_it_is_clipped_to_the_viewport() {
        let messages = vec![
            ChatMessage::User("hi".into()),
            tool_call(),
            ChatMessage::Reasoning {
                text: "a\nb".into(),
                expanded: false,
            },
        ];
        let ranges = [0..1, 1..4, 4..5];
        let areas = collapsible_hit_areas(
            &messages,
            |index| ranges.get(index).cloned(),
            0,
            5,
            Rect::new(0, 3, 80, 5),
        );
        assert_eq!(areas.len(), 2, "user messages are not collapsible");
        assert_eq!(areas[0].message_index, 1);
        assert_eq!(areas[0].rect.y, 4);
        assert_eq!(areas[0].rect.height, 3);
        assert_eq!(areas[1].message_index, 2);
    }
}
