//! The session recap as the TUI draws it: a strip of at most three rows at
//! the bottom of the activity area, and a detail panel with the full text.
//! Both are display-only and read their geometry from here, so what is drawn
//! and what a click hits never disagree.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::chrome::{dialog_rect, hint_line, panel_block};
use crate::display_text::{truncate_to_display_width, wrap_to_display_width};
use crate::theme::Theme;
use crate::types::RecapState;

const STRIP_ROWS: usize = 3;
const GLYPH: &str = "↳";
const LABEL: &str = "Recap";
/// ` ↳ Recap  `: the first row's label; later rows hang under its text.
const INDENT: usize = 10;
/// Below this many columns of text a recap reads as noise, so it is dropped.
const MIN_TEXT_WIDTH: usize = 12;
const HINT_SEPARATOR: &str = " · ";
const DETAIL_WIDTH: u16 = 76;
const DETAIL_MAX_HEIGHT: u16 = 18;

/// The strip's rows for `width` columns: at most three, never more than
/// `max_rows`, and none when the terminal is too narrow for them to read.
pub(crate) fn strip_lines(
    state: &RecapState,
    width: u16,
    max_rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let max_rows = max_rows.min(STRIP_ROWS);
    // One column stays free on the right, like the rest of the activity area.
    let text_width = usize::from(width).saturating_sub(INDENT + 1);
    if max_rows == 0 || text_width < MIN_TEXT_WIDTH {
        return Vec::new();
    }
    let (text, style, hint) = match state {
        RecapState::Hidden => return Vec::new(),
        RecapState::Requested | RecapState::Pending { .. } => (
            "summarizing this conversation…".to_string(),
            theme.muted_style(),
            None,
        ),
        RecapState::Ready {
            text, detail_open, ..
        } => (
            text.clone(),
            Style::default().fg(theme.text),
            // More to read than the strip holds: point at the full text,
            // unless it is already open.
            (!detail_open).then_some(("/recap", "view")),
        ),
        RecapState::Failed { message, .. } => (
            format!("couldn't summarize: {message}"),
            Style::default().fg(theme.warning),
            Some(("/recap", "retry")),
        ),
        RecapState::Notice(message) => (message.clone(), theme.muted_style(), None),
    };
    let always_hint = matches!(state, RecapState::Failed { .. });

    let mut rows = wrap_to_display_width(&text, text_width);
    let overflow = rows.len() > max_rows;
    rows.truncate(max_rows);
    let hint_width = |(key, verb): (&str, &str)| {
        UnicodeWidthStr::width(HINT_SEPARATOR)
            + UnicodeWidthStr::width(key)
            + 1
            + UnicodeWidthStr::width(verb)
    };
    // The hint needs a few columns of text beside it to read as a hint.
    let hint = hint
        .filter(|_| overflow || always_hint)
        .filter(|&hint| hint_width(hint) + 8 <= text_width);
    let hint_width = hint.map_or(0, hint_width);
    let room = text_width.saturating_sub(hint_width);
    let last_fits = rows
        .last()
        .is_some_and(|last| UnicodeWidthStr::width(last.as_str()) <= room);
    let mut hint_row = None;
    if !overflow && !last_fits && hint.is_some() && rows.len() < max_rows {
        hint_row = Some(rows.len());
        rows.push(String::new());
    } else if (overflow || !last_fits)
        && let Some(last) = rows.last_mut()
    {
        *last = truncate_to_display_width(&format!("{last}…"), room);
    }

    let last_row = rows.len().saturating_sub(1);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = if index == 0 {
                vec![
                    Span::raw(" "),
                    Span::styled(format!("{GLYPH} {LABEL}"), theme.accent_style()),
                    Span::raw("  "),
                ]
            } else {
                vec![Span::raw(" ".repeat(INDENT))]
            };
            if !row.is_empty() {
                spans.push(Span::styled(row, style));
            }
            if index == last_row
                && let Some((key, verb)) = hint
            {
                // A hint alone on its row starts at the text column.
                if hint_row != Some(index) {
                    spans.push(Span::styled(HINT_SEPARATOR, theme.muted_style()));
                }
                spans.extend(hint_line(theme, hint_width, &[(key, verb)]).spans);
            }
            Line::from(spans)
        })
        .collect()
}

/// The detail panel's rectangle inside `area`, or `None` when the recap is
/// not open or `area` is too small to show it.
pub(crate) fn detail_rect(area: Rect, state: &RecapState) -> Option<Rect> {
    detail_layout(area, state, None).map(|(rect, _)| rect)
}

/// Draws the detail panel and returns the rectangle it covers, which is what
/// mouse clicks are tested against.
pub(crate) fn render_detail(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &RecapState,
    theme: &Theme,
) -> Option<Rect> {
    let (popup, content) = detail_layout(area, state, Some(theme))?;
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(content).block(panel_block(theme, LABEL, theme.border)),
        popup,
    );
    Some(popup)
}

fn detail_layout(
    area: Rect,
    state: &RecapState,
    theme: Option<&Theme>,
) -> Option<(Rect, Vec<Line<'static>>)> {
    let RecapState::Ready {
        text,
        detail_open: true,
        ..
    } = state
    else {
        return None;
    };
    let width = DETAIL_WIDTH.min(area.width.saturating_sub(4));
    // `panel_block` spends a border and a padding cell on each side.
    let inner_width = width.saturating_sub(4);
    let max_rows = usize::from(
        DETAIL_MAX_HEIGHT
            .min(area.height.saturating_sub(2))
            .saturating_sub(2),
    );
    // The text needs at least one row, plus the gap and the hint below it.
    if usize::from(inner_width) < MIN_TEXT_WIDTH || max_rows < 3 {
        return None;
    }

    let mut rows = Vec::new();
    for paragraph in text.lines().map(str::trim) {
        if paragraph.is_empty() {
            if rows.last().is_some_and(|row: &String| !row.is_empty()) {
                rows.push(String::new());
            }
            continue;
        }
        rows.extend(wrap_to_display_width(paragraph, usize::from(inner_width)));
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let text_rows = max_rows - 2;
    if rows.len() > text_rows {
        rows.truncate(text_rows);
        let last = rows.last_mut().expect("at least one text row");
        *last = truncate_to_display_width(&format!("{last}…"), usize::from(inner_width));
    }

    let mut content = Vec::with_capacity(rows.len() + 2);
    if let Some(theme) = theme {
        content.extend(
            rows.into_iter()
                .map(|row| Line::from(Span::styled(row, Style::default().fg(theme.text)))),
        );
        content.push(Line::from(""));
        content.push(hint_line(
            theme,
            usize::from(inner_width),
            &[("Esc", "close")],
        ));
    } else {
        content.resize(rows.len() + 2, Line::from(""));
    }
    let popup = dialog_rect(area, width, content.len() as u16, DETAIL_MAX_HEIGHT);
    Some((popup, content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionAttachmentId;
    use orca_core::config::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    fn ready(text: &str, detail_open: bool) -> RecapState {
        let cursor = crate::surface_projection::test_surface_cursor(1);
        RecapState::Ready {
            attachment: SessionAttachmentId::new(1),
            source: orca_runtime::recap::RecapSourceFence {
                marker: orca_runtime::recap::RecapContentMarker {
                    thread_id: cursor.thread_id.clone(),
                    incarnation: cursor.incarnation.clone(),
                    completed_user_operations: 3,
                    evidence_digest: orca_runtime::surface::Sha256Digest::digest("view-test"),
                },
                cursor,
            },
            text: text.to_string(),
            usage: orca_runtime::recap::RecapUsage::Cached,
            detail_open,
        }
    }

    fn failed(message: &str) -> RecapState {
        let RecapState::Ready {
            attachment, source, ..
        } = ready("", false)
        else {
            unreachable!()
        };
        RecapState::Failed {
            attachment,
            source,
            message: message.to_string(),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    const LONG: &str = "已完成 TUI 视觉统一并接入鲸鱼欢迎页 👨‍👩‍👧‍👦；仍有 1 个 PR 待合并 \
        https://example.com/a/very/long/path/that/does/not/break/anywhere/naturally \
        and the release notes still need a pass before tagging.";

    #[test]
    fn strip_rows_fit_the_width_in_every_state() {
        let states = [
            ready(LONG, false),
            ready(LONG, true),
            ready("done", false),
            failed(LONG),
            RecapState::Requested,
            RecapState::Notice("available once the current turn finishes".to_string()),
        ];
        for state in &states {
            for width in 0..=100 {
                for max_rows in 0..=4 {
                    let lines = strip_lines(state, width, max_rows, &theme());
                    assert!(lines.len() <= max_rows.min(STRIP_ROWS), "{state:?} {width}");
                    for line in &lines {
                        assert!(
                            line.width() < usize::from(width),
                            "{width} cols: {:?}",
                            text(line)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_recap_that_fits_shows_whole_and_one_that_does_not_points_at_the_full_text() {
        let short = strip_lines(&ready("Fixed the relay drain.", false), 80, 3, &theme());
        assert_eq!(
            short.iter().map(text).collect::<Vec<_>>(),
            [" ↳ Recap  Fixed the relay drain."]
        );

        let long = strip_lines(&ready(LONG, false), 60, 3, &theme());
        assert_eq!(long.len(), 3);
        let rows = long.iter().map(text).collect::<Vec<_>>();
        assert!(rows[0].starts_with(" ↳ Recap  已完成"), "{rows:?}");
        assert!(rows[1].starts_with(&" ".repeat(INDENT)), "{rows:?}");
        assert!(rows[2].ends_with("… · /recap view"), "{rows:?}");

        // The same recap with its detail open needs no pointer to it.
        let open = strip_lines(&ready(LONG, true), 60, 3, &theme());
        assert!(text(&open[2]).ends_with('…'), "{:?}", text(&open[2]));
    }

    #[test]
    fn a_short_activity_area_gets_fewer_recap_rows_and_a_narrow_one_none() {
        assert_eq!(strip_lines(&ready(LONG, false), 60, 1, &theme()).len(), 1);
        assert!(strip_lines(&ready(LONG, false), 60, 0, &theme()).is_empty());
        assert!(strip_lines(&ready(LONG, false), 20, 3, &theme()).is_empty());
        assert!(strip_lines(&RecapState::Hidden, 80, 3, &theme()).is_empty());
    }

    #[test]
    fn a_failed_recap_always_offers_a_retry() {
        let rows = strip_lines(&failed("provider timed out"), 80, 3, &theme())
            .iter()
            .map(text)
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [" ↳ Recap  couldn't summarize: provider timed out · /recap retry"]
        );
    }

    #[test]
    fn the_detail_panel_wraps_the_full_text_inside_its_padding_and_matches_its_hit_rect() {
        let area = Rect::new(0, 0, 60, 24);
        let state = ready(&format!("{LONG}\n\nNext: tag v0.4.33."), true);
        let rect = detail_rect(area, &state).expect("detail rect");
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("terminal");
        let mut drawn = None;
        terminal
            .draw(|frame| drawn = render_detail(frame, area, &state, &theme()))
            .expect("draw");
        assert_eq!(drawn, Some(rect));

        let buffer = terminal.backend().buffer().clone();
        let rows = (rect.y..rect.y + rect.height)
            .map(|y| {
                (rect.x..rect.x + rect.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows[0].starts_with("╭ Recap "), "{rows:?}");
        // Border, padding, text, padding, border: nothing touches the frame.
        for row in &rows[1..rows.len() - 1] {
            assert!(row.starts_with("│ ") && row.ends_with(" │"), "{row:?}");
        }
        let body = rows.join("\n");
        assert!(body.contains("Next: tag v0.4.33."), "{body}");
        assert!(body.contains("Esc close"), "{body}");
        assert!(rect.height <= DETAIL_MAX_HEIGHT);
    }

    #[test]
    fn the_detail_panel_stays_closed_until_asked_and_off_a_tiny_terminal() {
        let area = Rect::new(0, 0, 80, 24);
        assert!(detail_rect(area, &ready("done", false)).is_none());
        assert!(detail_rect(area, &failed("boom")).is_none());
        assert!(detail_rect(Rect::new(0, 0, 14, 24), &ready("done", true)).is_none());
        assert!(detail_rect(Rect::new(0, 0, 80, 4), &ready("done", true)).is_none());
    }

    #[test]
    fn an_overlong_detail_is_cut_with_an_ellipsis_above_its_hint() {
        let text = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let area = Rect::new(0, 0, 80, 30);
        let state = ready(&text, true);
        let rect = detail_rect(area, &state).expect("detail rect");
        assert_eq!(rect.height, DETAIL_MAX_HEIGHT);
        let (_, content) = detail_layout(area, &state, Some(&theme())).expect("layout");
        let texts = content.iter().map(super::tests::text).collect::<Vec<_>>();
        assert_eq!(texts[texts.len() - 3], "line 13…");
        assert_eq!(texts[texts.len() - 1], "Esc close");
    }
}
