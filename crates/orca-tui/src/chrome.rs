//! Shared visual vocabulary for every TUI surface: one border, one selection
//! marker, one hint-line grammar, one transcript gutter. `ui.rs` composes
//! these instead of hand-rolling blocks and footers per dialog.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
use unicode_width::UnicodeWidthStr;

use crate::display_text::truncate_to_display_width;
use crate::theme::Theme;

pub(crate) const BORDER: BorderType = BorderType::Rounded;
pub(crate) const MARK_SELECTED: &str = "›";
pub(crate) const MARK_IDLE: &str = " ";
/// Transcript gutter: one leading space, a one-cell glyph, two spaces.
pub(crate) const GUTTER_WIDTH: usize = 4;
pub(crate) const GUTTER_CONTINUATION: &str = "    ";
#[cfg_attr(not(test), allow(dead_code))]
const RULE: &str = "─";
#[cfg_attr(not(test), allow(dead_code))]
const HINT_SEPARATOR: &str = " · ";

/// Rounded panel with a bold, accent-colored title. Every dialog and side
/// panel goes through here so titles and borders never drift apart.
pub(crate) fn panel_block(theme: &Theme, title: &str, accent: Color) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BORDER)
        .border_style(Style::default().fg(accent));
    if !title.is_empty() {
        block = block.title(Span::styled(
            format!(" {title} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    }
    let _ = theme;
    block
}

/// `↑↓ move · Enter confirm · Esc cancel`: keys in accent, verbs muted. Items
/// are dropped from the right until the line fits `width`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn hint_line(theme: &Theme, width: usize, items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (index, (keys, action)) in items.iter().enumerate() {
        let separator = if index == 0 { "" } else { HINT_SEPARATOR };
        let item_width = UnicodeWidthStr::width(separator)
            + UnicodeWidthStr::width(*keys)
            + 1
            + UnicodeWidthStr::width(*action);
        if used + item_width > width {
            break;
        }
        used += item_width;
        if !separator.is_empty() {
            spans.push(Span::styled(separator.to_string(), theme.muted_style()));
        }
        spans.push(Span::styled((*keys).to_string(), theme.accent_style()));
        spans.push(Span::styled(format!(" {action}"), theme.muted_style()));
    }
    Line::from(spans)
}

/// `› 1  label   detail`. The selected row gets the selection background so
/// it still reads on 16-color and monochrome terminals.
pub(crate) fn option_line(
    theme: &Theme,
    selected: bool,
    key: &str,
    label: &str,
    label_width: usize,
    detail: &str,
    width: usize,
) -> Line<'static> {
    let marker = if selected { MARK_SELECTED } else { MARK_IDLE };
    let marker_style = if selected {
        theme.accent_style().add_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };
    let key_style = if selected {
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let label_style = if selected {
        theme
            .selection_style()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let padding = label_width.saturating_sub(UnicodeWidthStr::width(label));
    let padded_label = format!("{label}{}", " ".repeat(padding));
    // An empty key (menus without numbers) collapses its column entirely.
    let key_cell = if key.is_empty() {
        String::new()
    } else {
        format!("{key}  ")
    };
    let used = UnicodeWidthStr::width(marker)
        + 1
        + UnicodeWidthStr::width(key_cell.as_str())
        + UnicodeWidthStr::width(padded_label.as_str())
        + 2;
    let detail = truncate_to_display_width(detail, width.saturating_sub(used));
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(key_cell, key_style),
        Span::styled(padded_label, label_style),
        Span::styled(format!("  {detail}"), theme.muted_style()),
    ])
}

/// A horizontal rule the width of the area; accent when the surface owns the
/// keyboard, dim otherwise.
pub(crate) fn rule_line(theme: &Theme, width: u16, focused: bool) -> Line<'static> {
    let style = if focused {
        theme.accent_style()
    } else {
        theme.dim_style()
    };
    Line::from(Span::styled(RULE.repeat(usize::from(width)), style))
}

/// Role gutter for transcript rows: `" ›  "`, `" ●  "`, always four cells.
pub(crate) fn gutter(theme: &Theme, glyph: &str, color: Color) -> Span<'static> {
    let _ = theme;
    Span::styled(format!(" {glyph}  "), Style::default().fg(color))
}

/// A centered rectangle sized to `content_rows` plus the two border rows,
/// clamped to `max_height` and to `area` (with a two-cell margin).
pub(crate) fn dialog_rect(area: Rect, width: u16, content_rows: u16, max_height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4)).max(1);
    let height = content_rows
        .saturating_add(2)
        .min(max_height)
        .min(area.height.saturating_sub(2))
        .max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    use super::*;
    use crate::theme::Theme;

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn hint_line_keeps_key_action_pairs_and_drops_from_the_right_when_narrow() {
        let items = [("↑↓", "move"), ("Enter", "confirm"), ("Esc", "cancel")];
        let wide = hint_line(&theme(), 80, &items);
        assert_eq!(text(&wide), "↑↓ move · Enter confirm · Esc cancel");
        let narrow = hint_line(&theme(), 24, &items);
        assert_eq!(text(&narrow), "↑↓ move · Enter confirm");
        assert_eq!(wide.spans[0].style.fg, Some(theme().border));
        assert_eq!(wide.spans[1].style.fg, Some(theme().muted));
    }

    #[test]
    fn option_line_marks_selection_with_background_and_pads_labels() {
        let selected = option_line(&theme(), true, "1", "Allow once", 12, "run it now", 60);
        assert_eq!(text(&selected), "› 1  Allow once    run it now");
        assert_eq!(selected.spans[2].style.bg, Some(theme().selection_bg));
        assert!(
            selected.spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let idle = option_line(&theme(), false, "2", "Deny", 12, "stop", 60);
        assert_eq!(text(&idle), "  2  Deny          stop");
        assert_eq!(idle.spans[2].style.bg, None);
    }

    #[test]
    fn option_line_without_a_key_collapses_the_key_column() {
        let line = option_line(&theme(), false, "", "/new", 8, "Start", 40);
        assert_eq!(text(&line), "  /new      Start");
    }

    #[test]
    fn option_line_truncates_detail_to_width() {
        let line = option_line(
            &theme(),
            false,
            "1",
            "Allow",
            5,
            "a very long explanation",
            24,
        );
        assert_eq!(text(&line), "  1  Allow  a very long…");
    }

    #[test]
    fn rule_line_spans_the_width_and_dims_when_unfocused() {
        let focused = rule_line(&theme(), 10, true);
        assert_eq!(text(&focused), "──────────");
        assert_eq!(focused.spans[0].style.fg, Some(theme().border));
        let unfocused = rule_line(&theme(), 4, false);
        assert!(
            unfocused.spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn gutter_is_four_cells_wide() {
        let span = gutter(&theme(), "›", theme().user);
        assert_eq!(span.content.as_ref(), " ›  ");
    }

    #[test]
    fn dialog_rect_fits_content_and_stays_inside_area() {
        let area = Rect::new(0, 5, 100, 40);
        let rect = dialog_rect(area, 72, 6, 20);
        assert_eq!((rect.width, rect.height), (72, 8));
        assert_eq!(rect.x, 14);
        assert_eq!(rect.y, 5 + (40 - 8) / 2);
        let clamped = dialog_rect(Rect::new(0, 0, 30, 10), 72, 30, 20);
        assert_eq!((clamped.width, clamped.height), (26, 8));
    }
}
