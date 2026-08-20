use std::ops::Range;

use crate::selection::SelectionPos;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchQuery {
    original: String,
    needle: String,
    case_sensitive: bool,
}

impl SearchQuery {
    pub(crate) fn new(query: &str) -> Self {
        let case_sensitive = query.chars().any(char::is_uppercase);
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.chars().flat_map(char::to_lowercase).collect()
        };
        Self {
            original: query.to_string(),
            needle,
            case_sensitive,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.original.is_empty()
    }

    pub(crate) fn find_ranges(&self, text: &str) -> Vec<Range<usize>> {
        if self.is_empty() {
            return Vec::new();
        }
        if self.case_sensitive {
            return text
                .match_indices(&self.needle)
                .map(|(start, matched)| start..start + matched.len())
                .collect();
        }

        let mut folded = String::new();
        let mut boundaries = Vec::with_capacity(text.chars().count() + 1);
        for (original_offset, character) in text.char_indices() {
            boundaries.push((folded.len(), original_offset));
            folded.extend(character.to_lowercase());
        }
        boundaries.push((folded.len(), text.len()));

        folded
            .match_indices(&self.needle)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                let original_start = boundaries
                    .binary_search_by_key(&start, |(folded, _)| *folded)
                    .ok()
                    .map(|index| boundaries[index].1)?;
                let original_end = boundaries
                    .binary_search_by_key(&end, |(folded, _)| *folded)
                    .ok()
                    .map(|index| boundaries[index].1)?;
                Some(original_start..original_end)
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct TranscriptLineIdentity {
    pub(crate) message_revision: u64,
    pub(crate) line_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSearchMatch {
    pub(crate) start: SelectionPos,
    pub(crate) end: SelectionPos,
    pub(crate) line_identity: TranscriptLineIdentity,
    pub(crate) byte_range: Range<usize>,
}

impl TranscriptSearchMatch {
    pub(crate) fn new(
        start: SelectionPos,
        end: SelectionPos,
        line_identity: TranscriptLineIdentity,
        byte_range: Range<usize>,
    ) -> Self {
        Self {
            start,
            end,
            line_identity,
            byte_range,
        }
    }

    pub(crate) fn last_covered_row(&self) -> usize {
        if self.end.row > self.start.row && self.end.col == 0 {
            self.end.row - 1
        } else {
            self.end.row
        }
    }

    pub(crate) fn cols_on_row(&self, row: usize) -> Option<(usize, Option<usize>)> {
        let last_row = self.last_covered_row();
        if row < self.start.row || row > last_row {
            return None;
        }
        if self.start.row == self.end.row {
            return (self.start.col < self.end.col).then_some((self.start.col, Some(self.end.col)));
        }
        if row == self.start.row {
            Some((self.start.col, None))
        } else if row == last_row {
            if self.end.col == 0 {
                Some((0, None))
            } else {
                Some((0, Some(self.end.col)))
            }
        } else {
            Some((0, None))
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptSearchState {
    pub(crate) open: bool,
    query: String,
    cursor: usize,
    matches: Vec<TranscriptSearchMatch>,
    active: Option<usize>,
    prepared_generation: Option<u64>,
    prepared_query: String,
    #[cfg(test)]
    scan_count: usize,
}

impl TranscriptSearchState {
    pub(crate) fn open_new(&mut self) {
        if self.open {
            return;
        }
        self.open = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.active = None;
        self.invalidate_prepared();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    pub(crate) fn insert_char(&mut self, character: char) {
        self.query.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.invalidate_prepared();
    }

    pub(crate) fn insert_paste(&mut self, pasted: &str) {
        let normalized = pasted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.query.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.invalidate_prepared();
    }

    pub(crate) fn backspace(&mut self) -> bool {
        let Some(previous) = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return false;
        };
        self.query.drain(previous..self.cursor);
        self.cursor = previous;
        self.invalidate_prepared();
        true
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = self.query[self.cursor..]
            .chars()
            .next()
            .map_or(self.query.len(), |character| {
                self.cursor + character.len_utf8()
            });
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.query.len();
    }

    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.active = None;
        self.invalidate_prepared();
    }

    #[cfg(test)]
    pub(crate) fn replace_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.cursor = self.query.len();
        self.invalidate_prepared();
    }

    pub(crate) fn refresh_with(
        &mut self,
        generation: u64,
        viewport_base: usize,
        search: impl FnOnce(&SearchQuery) -> Vec<TranscriptSearchMatch>,
    ) {
        if self.prepared_generation == Some(generation) && self.prepared_query == self.query {
            return;
        }

        let same_query = self.prepared_query == self.query;
        let previous = same_query.then(|| self.active_match().cloned()).flatten();
        let query = SearchQuery::new(&self.query);
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            #[cfg(test)]
            {
                self.scan_count += 1;
            }
            search(&query)
        };

        let active = if matches.is_empty() {
            None
        } else if let Some(previous) = previous.as_ref() {
            matches
                .iter()
                .position(|found| {
                    found.line_identity == previous.line_identity
                        && found.byte_range == previous.byte_range
                })
                .or_else(|| {
                    let next = matches.partition_point(|found| found.start < previous.start);
                    Some(if next == matches.len() { 0 } else { next })
                })
        } else {
            let next = matches.partition_point(|found| found.start.row < viewport_base);
            Some(if next == matches.len() { 0 } else { next })
        };

        self.matches = matches;
        self.active = active;
        self.prepared_generation = Some(generation);
        self.prepared_query.clone_from(&self.query);
    }

    pub(crate) fn next(&mut self) -> Option<&TranscriptSearchMatch> {
        if self.matches.is_empty() {
            self.active = None;
            return None;
        }
        self.active = Some(match self.active {
            Some(index) => (index + 1) % self.matches.len(),
            None => 0,
        });
        self.active_match()
    }

    pub(crate) fn previous(&mut self) -> Option<&TranscriptSearchMatch> {
        if self.matches.is_empty() {
            self.active = None;
            return None;
        }
        self.active = Some(match self.active {
            Some(0) | None => self.matches.len() - 1,
            Some(index) => index - 1,
        });
        self.active_match()
    }

    pub(crate) fn active_match(&self) -> Option<&TranscriptSearchMatch> {
        self.active.and_then(|index| self.matches.get(index))
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub(crate) fn active_ordinal(&self) -> Option<usize> {
        self.active.map(|index| index + 1)
    }

    pub(crate) fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub(crate) fn clear_matches(&mut self, generation: u64) {
        self.matches.clear();
        self.active = None;
        self.prepared_generation = Some(generation);
        self.prepared_query.clone_from(&self.query);
    }

    pub(crate) fn visible_matches(
        &self,
        start_row: usize,
        end_row: usize,
    ) -> impl Iterator<Item = (usize, &TranscriptSearchMatch)> {
        self.matches
            .iter()
            .enumerate()
            .skip_while(move |(_, found)| found.last_covered_row() < start_row)
            .take_while(move |(_, found)| found.start.row < end_row)
    }

    fn invalidate_prepared(&mut self) {
        self.prepared_generation = None;
    }

    #[cfg(test)]
    pub(crate) fn scan_count_for_test(&self) -> usize {
        self.scan_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_at(
        row: usize,
        col: usize,
        revision: u64,
        bytes: Range<usize>,
    ) -> TranscriptSearchMatch {
        TranscriptSearchMatch {
            start: SelectionPos { row, col },
            end: SelectionPos {
                row,
                col: col + (bytes.end - bytes.start),
            },
            line_identity: TranscriptLineIdentity {
                message_revision: revision,
                line_index: 0,
            },
            byte_range: bytes,
        }
    }

    #[test]
    fn lowercase_query_is_case_insensitive_and_uppercase_query_is_sensitive() {
        let insensitive = SearchQuery::new("error");
        assert_eq!(insensitive.find_ranges("ERROR error"), vec![0..5, 6..11]);

        let sensitive = SearchQuery::new("Error");
        assert_eq!(sensitive.find_ranges("ERROR Error error"), vec![6..11]);
    }

    #[test]
    fn unicode_case_folding_maps_matches_back_to_original_boundaries() {
        let query = SearchQuery::new("ä");
        let text = "Ärger ä";
        let ranges = query.find_ranges(text);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].clone()], "Ä");
        assert_eq!(&text[ranges[1].clone()], "ä");
        assert!(
            ranges
                .iter()
                .all(|range| text_boundary_or_empty(text, range))
        );
    }

    #[test]
    fn repeated_matches_are_non_overlapping() {
        assert_eq!(SearchQuery::new("aa").find_ranges("aaaa"), vec![0..2, 2..4]);
        assert!(SearchQuery::new("").find_ranges("anything").is_empty());
    }

    #[test]
    fn query_editing_uses_utf8_byte_cursor_and_paste_normalizes_lines() {
        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.insert_char('中');
        search.insert_char('a');
        search.move_left();
        search.insert_char('文');
        assert_eq!(search.query(), "中文a");
        assert_eq!(search.cursor(), "中文".len());
        assert!(search.backspace());
        assert_eq!(search.query(), "中a");
        search.insert_paste("one\r\ntwo\nthree");
        assert_eq!(search.query(), "中one two threea");
    }

    #[test]
    fn refresh_preserves_active_identity_and_selects_nearest_following_match() {
        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("hit");
        let first = match_at(2, 0, 10, 0..3);
        let second = match_at(8, 0, 20, 0..3);
        search.refresh_with(1, 5, |_| vec![first.clone(), second.clone()]);
        assert_eq!(search.active_match(), Some(&second));

        search.next();
        assert_eq!(search.active_match(), Some(&first));
        search.refresh_with(2, 0, |_| vec![second.clone()]);
        assert_eq!(search.active_match(), Some(&second));
    }

    #[test]
    fn next_and_previous_wrap_without_rescanning() {
        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("x");
        search.refresh_with(1, 0, |_| {
            vec![match_at(1, 0, 1, 0..1), match_at(4, 0, 2, 0..1)]
        });
        let scans = search.scan_count;
        assert_eq!(search.next().map(|found| found.start.row), Some(4));
        assert_eq!(search.next().map(|found| found.start.row), Some(1));
        assert_eq!(search.previous().map(|found| found.start.row), Some(4));
        assert_eq!(search.scan_count, scans);
    }

    fn text_boundary_or_empty(text: &str, range: &Range<usize>) -> bool {
        range.is_empty()
            || (range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end))
    }
}

use crate::types::{AppState, AppStatus, PanelMode};

impl AppState {
    pub(crate) fn open_transcript_search(&mut self) {
        if self.panel_mode == PanelMode::Conversation
            && matches!(
                self.status,
                AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
            )
        {
            self.transcript_search.open_new();
        }
    }

    pub(crate) fn close_transcript_search(&mut self) {
        self.transcript_search.close();
    }

    #[cfg(test)]
    pub(crate) fn replace_transcript_search_query(&mut self, query: &str) {
        self.transcript_search.replace_query(query);
    }

    pub(crate) fn refresh_transcript_search(&mut self) {
        let generation = self.transcript_render_cache.content_generation();
        let viewport_base = self.viewport_base_row;
        let live_start = self.flushed_count.min(self.messages.len());
        let cache = &self.transcript_render_cache;
        self.transcript_search
            .refresh_with(generation, viewport_base, |query| {
                cache.search(live_start, query)
            });
    }

    pub(crate) fn search_next(&mut self) {
        self.refresh_transcript_search();
        let Some(found) = self.transcript_search.next().cloned() else {
            return;
        };
        self.scroll_offset = self.transcript_render_cache.reveal_offset(
            self.flushed_count,
            self.scroll_offset,
            self.visible_height,
            &found,
        );
        self.auto_scroll = false;
    }

    pub(crate) fn search_previous(&mut self) {
        self.refresh_transcript_search();
        let Some(found) = self.transcript_search.previous().cloned() else {
            return;
        };
        self.scroll_offset = self.transcript_render_cache.reveal_offset(
            self.flushed_count,
            self.scroll_offset,
            self.visible_height,
            &found,
        );
        self.auto_scroll = false;
    }
}
