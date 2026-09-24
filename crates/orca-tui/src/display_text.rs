use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    if text_width <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let ellipsis = "…";
    let content_width = max_width.saturating_sub(UnicodeWidthStr::width(ellipsis));
    let mut truncated = String::new();
    let mut width = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > content_width {
            break;
        }
        truncated.push_str(grapheme);
        width += grapheme_width;
    }
    truncated.push_str(ellipsis);
    truncated
}

/// `text` broken into rows at most `width` columns wide: at whitespace where
/// it can, on either side of a wide (CJK, emoji) grapheme, and inside a word
/// only when the word alone is wider than a row -- never inside a grapheme.
/// Runs of whitespace, newlines included, collapse to one space.
pub(crate) fn wrap_to_display_width(text: &str, width: usize) -> Vec<String> {
    let mut rows = RowFiller {
        width,
        rows: Vec::new(),
        row: String::new(),
        row_width: 0,
    };
    if width == 0 {
        return rows.rows;
    }
    let mut word = String::new();
    let mut word_width = 0;
    let mut space_before = false;
    for grapheme in text.graphemes(true) {
        if grapheme.chars().all(char::is_whitespace) {
            if !word.is_empty() {
                rows.place(&std::mem::take(&mut word), word_width, space_before);
                word_width = 0;
            }
            space_before = true;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if grapheme_width < 2 {
            word.push_str(grapheme);
            word_width += grapheme_width;
            continue;
        }
        if !word.is_empty() {
            rows.place(&std::mem::take(&mut word), word_width, space_before);
            word_width = 0;
            space_before = false;
        }
        rows.place(grapheme, grapheme_width, space_before);
        space_before = false;
    }
    if !word.is_empty() {
        rows.place(&word, word_width, space_before);
    }
    rows.finish()
}

struct RowFiller {
    width: usize,
    rows: Vec<String>,
    row: String,
    row_width: usize,
}

impl RowFiller {
    /// Places one unit that only breaks where it has to: whole on this row,
    /// else whole on the next, else split between graphemes.
    fn place(&mut self, unit: &str, unit_width: usize, space_before: bool) {
        let separator = usize::from(space_before && !self.row.is_empty());
        if self.row_width + separator + unit_width <= self.width {
            if separator == 1 {
                self.row.push(' ');
            }
            self.row.push_str(unit);
            self.row_width += separator + unit_width;
            return;
        }
        if unit_width <= self.width {
            self.break_row();
            self.row.push_str(unit);
            self.row_width = unit_width;
            return;
        }
        if separator == 1 && self.row_width + 1 < self.width {
            self.row.push(' ');
            self.row_width += 1;
        }
        for grapheme in unit.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if grapheme_width > self.width {
                continue;
            }
            if self.row_width + grapheme_width > self.width {
                self.break_row();
            }
            self.row.push_str(grapheme);
            self.row_width += grapheme_width;
        }
    }

    fn break_row(&mut self) {
        if !self.row.is_empty() {
            let row = std::mem::take(&mut self.row);
            self.rows.push(row.trim_end().to_string());
        }
        self.row_width = 0;
    }

    fn finish(mut self) -> Vec<String> {
        self.break_row();
        self.rows
    }
}

pub(crate) fn compact_long_text(text: &str, line_width: usize, max_lines: usize) -> String {
    const LONG_TEXT_CHAR_THRESHOLD: usize = 1000;
    const LONG_TEXT_LINE_THRESHOLD: usize = 8;

    let char_count = text.chars().count();
    if char_count <= LONG_TEXT_CHAR_THRESHOLD && text.lines().count() <= LONG_TEXT_LINE_THRESHOLD {
        return text.to_string();
    }

    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let suffix = format!(" [{} chars]", char_count);
    let total_width = line_width.saturating_mul(max_lines);
    let body_width = total_width.saturating_sub(UnicodeWidthStr::width(suffix.as_str()));
    format!(
        "{}{}",
        truncate_to_display_width(&normalized, body_width),
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_fills_rows_and_breaks_between_wide_characters() {
        assert_eq!(
            wrap_to_display_width("已完成 TUI 视觉统一并接入鲸鱼欢迎页；仍有 1 个 PR", 23),
            ["已完成 TUI 视觉统一并接", "入鲸鱼欢迎页；仍有 1 个", "PR"]
        );
        assert_eq!(
            wrap_to_display_width("fix the relay\n\n drain now", 9),
            ["fix the", "relay", "drain now"]
        );
        assert_eq!(
            wrap_to_display_width("see https://example.com/a/long/path ok", 12),
            ["see https://", "example.com/", "a/long/path", "ok"]
        );
        assert!(wrap_to_display_width("anything", 0).is_empty());
        assert!(wrap_to_display_width("   ", 10).is_empty());
    }

    #[test]
    fn wrapped_rows_never_exceed_the_width_or_split_a_grapheme() {
        let text = "e\u{301}clair 👨‍👩‍👧‍👦 家族 1️⃣ keycap 目标内容很长 averyveryverylongtoken";
        for width in 1..=30 {
            let rows = wrap_to_display_width(text, width);
            for row in &rows {
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "{width}: {row:?}"
                );
            }
            let joined = rows.concat();
            let kept = joined.graphemes(true).collect::<Vec<_>>();
            for grapheme in ["e\u{301}", "👨‍👩‍👧‍👦", "1️⃣"] {
                if UnicodeWidthStr::width(grapheme) <= width {
                    assert!(
                        kept.contains(&grapheme),
                        "{width}: {grapheme:?} in {rows:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_handles_ascii_and_cjk_display_width() {
        assert_eq!(truncate_to_display_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_display_width("目标内容很长", 5), "目标…");
        assert_eq!(truncate_to_display_width("目标", 4), "目标");
        assert_eq!(truncate_to_display_width("anything", 0), "");
    }

    #[test]
    fn truncation_never_splits_extended_graphemes() {
        for grapheme in ["e\u{301}", "👍🏽", "👨‍👩‍👧‍👦", "1️⃣"] {
            let grapheme_width = unicode_width::UnicodeWidthStr::width(grapheme);
            assert_eq!(
                truncate_to_display_width(&format!("{grapheme}x"), grapheme_width + 1),
                format!("{grapheme}x"),
                "{grapheme:?}"
            );
            assert_eq!(
                truncate_to_display_width(&format!("{grapheme}x"), grapheme_width),
                "…",
                "{grapheme:?}"
            );
        }
    }

    #[test]
    fn truncation_keeps_combining_emoji_and_keycap_clusters_atomic() {
        assert_eq!(truncate_to_display_width("e\u{301}x", 1), "…");
        assert_eq!(truncate_to_display_width("e\u{301}xy", 2), "e\u{301}…");
        assert_eq!(truncate_to_display_width("👍🏽x", 2), "…");
        assert_eq!(truncate_to_display_width("👍🏽xy", 3), "👍🏽…");
        assert_eq!(truncate_to_display_width("1️⃣x", 2), "…");
        assert_eq!(truncate_to_display_width("1️⃣xy", 3), "1️⃣…");
    }

    #[test]
    fn compact_long_text_bounds_transcript_rows_and_keeps_size() {
        let text = "目标内容".repeat(300);
        let compact = compact_long_text(&text, 40, 3);

        assert!(UnicodeWidthStr::width(compact.as_str()) <= 120);
        assert!(compact.contains("[1200 chars]"));
        assert!(compact.contains('…'));
    }
}
