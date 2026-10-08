use ratatui_textarea::{CursorMove, DataCursor, Input, Key, TextArea};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Theme;
use crate::vim::VimState;

pub(crate) const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;
pub(crate) const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;

pub(crate) fn make_textarea<'a>(vim_state: &VimState, theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    configure_textarea(&mut textarea, vim_state, theme);
    textarea
}

pub(crate) fn make_textarea_with_text<'a>(
    text: &str,
    vim_state: &VimState,
    theme: &Theme,
) -> TextArea<'a> {
    make_textarea_with_text_at_cursor(text, text.len(), vim_state, theme)
}

pub(crate) fn make_textarea_with_text_at_cursor<'a>(
    text: &str,
    cursor: usize,
    vim_state: &VimState,
    theme: &Theme,
) -> TextArea<'a> {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(str::to_string).collect()
    };
    let mut textarea = TextArea::from(lines);
    configure_textarea(&mut textarea, vim_state, theme);
    let cursor = cursor.min(text.len());
    let cursor = if text.is_char_boundary(cursor) {
        cursor
    } else {
        (0..cursor)
            .rev()
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(0)
    };
    let before_cursor = &text[..cursor];
    let row = before_cursor.bytes().filter(|byte| *byte == b'\n').count();
    let column = before_cursor
        .rsplit_once('\n')
        .map_or(before_cursor, |(_, line)| line)
        .chars()
        .count();
    textarea.move_cursor(CursorMove::Jump(
        row.min(u16::MAX as usize) as u16,
        column.min(u16::MAX as usize) as u16,
    ));
    textarea
}

fn configure_textarea(textarea: &mut TextArea, vim_state: &VimState, theme: &Theme) {
    textarea.set_placeholder_text("Message Orca…  / commands · @ files · $ skills");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    vim_state.configure_textarea(textarea, theme);
}

pub(crate) fn textarea_text(textarea: &TextArea) -> String {
    textarea.lines().join("\n")
}

pub(crate) fn textarea_cursor_byte_index(textarea: &TextArea) -> usize {
    let DataCursor(row, column) = textarea.cursor();
    let mut cursor = 0usize;
    for (index, line) in textarea.lines().iter().enumerate() {
        if index == row {
            cursor += line
                .char_indices()
                .nth(column)
                .map_or(line.len(), |(offset, _)| offset);
            return cursor;
        }
        cursor += line.len() + 1;
    }
    cursor
}

/// `TextArea::input` for the composer. Left, Right, Up and Down, and their
/// Ctrl+B, Ctrl+F, Ctrl+P and Ctrl+N forms, with or without Shift, move
/// through `move_composer_cursor`.
pub(crate) fn composer_input(textarea: &mut TextArea, input: Input) -> bool {
    let movement = match (input.key, input.ctrl, input.alt) {
        (Key::Right, false, false) | (Key::Char('f'), true, false) => CursorMove::Forward,
        (Key::Left, false, false) | (Key::Char('b'), true, false) => CursorMove::Back,
        (Key::Up, false, false) | (Key::Char('p'), true, false) => CursorMove::Up,
        (Key::Down, false, false) | (Key::Char('n'), true, false) => CursorMove::Down,
        _ => return textarea.input(input),
    };
    move_composer_cursor(textarea, movement, Some(input.shift));
    false
}

/// Moves the composer's cursor so it never stops in front of a character
/// that takes no screen column, such as the variation selector in ⚠️, the
/// jamo of a Hangul syllable written in parts or a zero-width space.
/// ratatui-textarea moves by screen column and can stop there, and what is
/// typed next then lands inside a character.
/// - Left and right step one character and cross a line break only at a
///   line's true end or start, as tui-textarea 0.7 did.
/// - Up and down keep the screen column, but a cursor that lands inside a
///   character goes on to that character's end.
/// - Other moves are the textarea's own.
///
/// `shift` is a key press's Shift, which starts or keeps a selection while
/// its absence drops one. vim passes `None`, keeping the selection as it is.
pub(crate) fn move_composer_cursor(
    textarea: &mut TextArea,
    movement: CursorMove,
    shift: Option<bool>,
) {
    let before = textarea.cursor();
    let (row, col) = match movement {
        CursorMove::Forward | CursorMove::Back => {
            let Some(target) = one_character_on(textarea.lines(), before, movement) else {
                return;
            };
            match shift {
                Some(true) if !textarea.is_selecting() => textarea.start_selection(),
                Some(false) => textarea.cancel_selection(),
                _ => {}
            }
            target
        }
        CursorMove::Up | CursorMove::Down => {
            match shift {
                Some(shift) => {
                    let key = if movement == CursorMove::Up {
                        Key::Up
                    } else {
                        Key::Down
                    };
                    textarea.input(Input {
                        key,
                        shift,
                        ..Input::default()
                    });
                }
                None => textarea.move_cursor(movement),
            }
            let DataCursor(row, col) = textarea.cursor();
            let end = out_of_character(&textarea.lines()[row], col);
            if textarea.cursor() == before || end == col {
                return;
            }
            (row, end)
        }
        other => return textarea.move_cursor(other),
    };
    match (u16::try_from(row), u16::try_from(col)) {
        (Ok(row), Ok(col)) => textarea.move_cursor(CursorMove::Jump(row, col)),
        // `Jump` stops at u16; on a line that long the textarea's own move has to do.
        _ if movement != CursorMove::Up && movement != CursorMove::Down => {
            textarea.move_cursor(movement)
        }
        _ => {}
    }
}

/// Where tui-textarea 0.7 put the cursor one character left or right of
/// `cursor`, or `None` at the very start or end of the text.
fn one_character_on(
    lines: &[String],
    DataCursor(row, col): DataCursor,
    movement: CursorMove,
) -> Option<(usize, usize)> {
    match movement {
        CursorMove::Forward if col < lines[row].chars().count() => Some((row, col + 1)),
        CursorMove::Forward if row + 1 < lines.len() => Some((row + 1, 0)),
        CursorMove::Back if col > 0 => Some((row, col - 1)),
        CursorMove::Back if row > 0 => Some((row - 1, lines[row - 1].chars().count())),
        _ => None,
    }
}

/// `col`, or the end of the character (grapheme cluster) `col` falls inside.
fn out_of_character(line: &str, col: usize) -> usize {
    let mut start = 0;
    for grapheme in line.graphemes(true) {
        let end = start + grapheme.chars().count();
        if col < end {
            return if col > start { end } else { col };
        }
        start = end;
    }
    col
}

pub(crate) fn insert_pasted_text(textarea: &mut TextArea, pasted: &str) -> bool {
    if pasted.is_empty() {
        return false;
    }
    textarea.insert_str(pasted)
}

pub(crate) fn insert_composer_paste(
    textarea: &mut TextArea,
    pending_pastes: &mut Vec<(String, String)>,
    pasted: &str,
) -> bool {
    if pasted.is_empty() {
        return false;
    }

    let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let char_count = pasted.chars().count();
    if char_count <= LARGE_PASTE_CHAR_THRESHOLD {
        return textarea.insert_str(&pasted);
    }

    let visible_text = textarea_text(textarea);
    retain_active_pending_pastes(&visible_text, pending_pastes);
    let placeholder = next_large_paste_placeholder(pending_pastes, char_count);
    if !textarea.insert_str(&placeholder) {
        return false;
    }
    pending_pastes.push((placeholder, pasted));
    true
}

fn next_large_paste_placeholder(pending_pastes: &[(String, String)], char_count: usize) -> String {
    let base = format!("[Pasted Content {char_count} chars]");
    let prefix = format!("{base} #");
    let mut max_suffix = 0usize;

    for (placeholder, _) in pending_pastes {
        if placeholder == &base {
            max_suffix = max_suffix.max(1);
        } else if let Some(suffix) = placeholder.strip_prefix(&prefix)
            && let Ok(value) = suffix.parse::<usize>()
        {
            max_suffix = max_suffix.max(value);
        }
    }

    if max_suffix == 0 {
        base
    } else {
        format!("{base} #{}", max_suffix + 1)
    }
}

pub(crate) fn expand_pending_pastes(
    visible_text: &str,
    pending_pastes: &[(String, String)],
) -> String {
    let mut replacements = locate_pending_pastes(visible_text, pending_pastes)
        .into_iter()
        .map(|(start, end, index)| (start, end, pending_pastes[index].1.as_str()))
        .collect::<Vec<_>>();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);

    let mut expanded = visible_text.to_string();
    for (start, end, actual) in replacements.into_iter().rev() {
        expanded.replace_range(start..end, actual);
    }
    expanded
}

pub(crate) fn expand_pending_pastes_with_bindings(
    visible_text: &str,
    pending_pastes: &[(String, String)],
    bindings: &mut orca_runtime::mentions::MentionBindings,
) -> String {
    let mut replacements = locate_pending_pastes(visible_text, pending_pastes)
        .into_iter()
        .map(|(start, end, index)| (start, end, pending_pastes[index].1.as_str()))
        .collect::<Vec<_>>();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);

    let mut expanded = visible_text.to_string();
    bindings.reconcile(&expanded);
    for (start, end, actual) in replacements.into_iter().rev() {
        expanded.replace_range(start..end, actual);
        bindings.reconcile(&expanded);
    }
    expanded
}

pub(crate) fn retain_active_pending_pastes(
    visible_text: &str,
    pending_pastes: &mut Vec<(String, String)>,
) {
    let active_placeholders = locate_pending_pastes(visible_text, pending_pastes)
        .into_iter()
        .map(|(_, _, index)| pending_pastes[index].0.clone())
        .collect::<Vec<_>>();
    pending_pastes.retain(|(placeholder, _)| active_placeholders.contains(placeholder));
}

pub(crate) fn locate_pending_pastes(
    visible_text: &str,
    pending_pastes: &[(String, String)],
) -> Vec<(usize, usize, usize)> {
    let mut indices = (0..pending_pastes.len()).collect::<Vec<_>>();
    indices.sort_unstable_by_key(|index| std::cmp::Reverse(pending_pastes[*index].0.len()));

    let mut located = Vec::new();
    for index in indices {
        let placeholder = &pending_pastes[index].0;
        if let Some((start, _)) = visible_text.match_indices(placeholder).find(|(start, _)| {
            let end = *start + placeholder.len();
            located
                .iter()
                .all(|(other_start, other_end, _)| end <= *other_start || *start >= *other_end)
        }) {
            located.push((start, start + placeholder.len(), index));
        }
    }
    located
}

pub(crate) fn make_setup_textarea<'a>(theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("sk-...");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_mask_char('*');
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(crate::chrome::BORDER)
            .title(" API Key ")
            .border_style(ratatui::style::Style::default().fg(theme.border)),
    );
    textarea
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use crate::vim::VimState;
    use orca_core::config::ThemeName;

    fn textarea() -> TextArea<'static> {
        make_textarea(&VimState::new(false), &Theme::named(ThemeName::Dark))
    }

    #[test]
    fn large_paste_is_collapsed_and_expands_without_losing_content() {
        let mut textarea = textarea();
        let mut pending = Vec::new();
        let pasted = "alpha\n".repeat(200);

        assert!(insert_composer_paste(&mut textarea, &mut pending, &pasted));
        assert_eq!(
            textarea_text(&textarea),
            format!("[Pasted Content {} chars]", pasted.chars().count())
        );
        assert_eq!(
            expand_pending_pastes(&textarea_text(&textarea), &pending),
            pasted
        );
    }

    #[test]
    fn paste_at_threshold_remains_editable_text() {
        let mut textarea = textarea();
        let mut pending = Vec::new();
        let pasted = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD);

        assert!(insert_composer_paste(&mut textarea, &mut pending, &pasted));
        assert_eq!(textarea_text(&textarea), pasted);
        assert!(pending.is_empty());
    }

    #[test]
    fn equal_sized_large_pastes_receive_distinct_placeholders() {
        let mut textarea = textarea();
        let mut pending = Vec::new();
        let pasted = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);

        insert_composer_paste(&mut textarea, &mut pending, &pasted);
        textarea.insert_str(" and ");
        insert_composer_paste(&mut textarea, &mut pending, &pasted);

        let base = format!("[Pasted Content {} chars]", pasted.chars().count());
        assert_eq!(textarea_text(&textarea), format!("{base} and {base} #2"));
        assert_eq!(
            expand_pending_pastes(&textarea_text(&textarea), &pending),
            format!("{pasted} and {pasted}")
        );
    }

    #[test]
    fn removed_placeholder_is_not_expanded_elsewhere() {
        let pending = vec![(
            "[Pasted Content 1001 chars]".to_string(),
            "secret".to_string(),
        )];

        assert_eq!(expand_pending_pastes("keep this", &pending), "keep this");
    }

    #[test]
    fn removed_base_placeholder_is_not_confused_with_suffixed_placeholder() {
        let pending = vec![
            (
                "[Pasted Content 1001 chars]".to_string(),
                "first".to_string(),
            ),
            (
                "[Pasted Content 1001 chars] #2".to_string(),
                "second".to_string(),
            ),
        ];

        assert_eq!(
            expand_pending_pastes("[Pasted Content 1001 chars] #2", &pending),
            "second"
        );
    }

    #[test]
    fn placeholder_like_text_inside_paste_is_not_recursively_expanded() {
        let first = "contains [Pasted Content 1002 chars] literally";
        let pending = vec![
            ("[Pasted Content 1001 chars]".to_string(), first.to_string()),
            (
                "[Pasted Content 1002 chars]".to_string(),
                "second".to_string(),
            ),
        ];
        let visible = "[Pasted Content 1001 chars] / [Pasted Content 1002 chars]";

        assert_eq!(
            expand_pending_pastes(visible, &pending),
            format!("{first} / second")
        );
    }

    #[test]
    fn cursor_byte_index_tracks_unicode_and_multiple_lines() {
        let mut textarea = make_textarea_with_text_at_cursor(
            "first\n你好吗 @src",
            "first\n你好".len(),
            &VimState::new(false),
            &Theme::named(ThemeName::Dark),
        );

        assert_eq!(textarea_cursor_byte_index(&textarea), "first\n你好".len());
        textarea.move_cursor(CursorMove::End);
        assert_eq!(
            textarea_cursor_byte_index(&textarea),
            "first\n你好吗 @src".len()
        );
    }
}
