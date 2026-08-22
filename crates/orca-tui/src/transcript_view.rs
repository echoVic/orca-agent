#[cfg(test)]
use std::cell::Cell;
use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    mem,
    time::{Duration, Instant},
};

use ratatui::layout::Alignment;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::{Line, Span, StyledGrapheme};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::selection::{SelectionPos, TranscriptSelection, slice_row_by_columns};
use crate::terminal_capabilities::TerminalColorLevel;
use crate::theme::Theme;
use crate::transcript_search::{SearchQuery, TranscriptLineIdentity, TranscriptSearchMatch};
use crate::types::ChatMessage;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug)]
struct StyleRun {
    start: usize,
    style: Style,
}

#[derive(Clone, Debug)]
struct CompactWrappedLine {
    text: String,
    row_boundaries: Vec<usize>,
    /// Whitespace the word-wrapper dropped at each row boundary
    /// (`wrap_gaps[i]` sits between row `i` and row `i + 1`). Rendering never
    /// shows it, but clipboard extraction re-inserts it so soft-wrapped prose
    /// copies as "foo bar", not "foobar".
    wrap_gaps: Vec<String>,
    style_runs: Vec<StyleRun>,
    alignment: Option<Alignment>,
}

impl CompactWrappedLine {
    fn new(alignment: Option<Alignment>) -> Self {
        Self {
            text: String::new(),
            row_boundaries: vec![0],
            wrap_gaps: Vec::new(),
            style_runs: Vec::new(),
            alignment,
        }
    }

    fn note_gap(&mut self, symbol: &str) {
        if let Some(last) = self.wrap_gaps.last_mut() {
            last.push_str(symbol);
        }
    }

    fn push_row(&mut self, graphemes: Vec<StyledGrapheme<'_>>) {
        for grapheme in graphemes {
            if self
                .style_runs
                .last()
                .is_none_or(|run| run.style != grapheme.style)
            {
                self.style_runs.push(StyleRun {
                    start: self.text.len(),
                    style: grapheme.style,
                });
            }
            self.text.push_str(grapheme.symbol);
        }
        self.row_boundaries.push(self.text.len());
        self.wrap_gaps.push(String::new());
    }

    fn row_count(&self) -> usize {
        self.row_boundaries.len().saturating_sub(1)
    }

    fn materialize_rows(&self, start: usize, end: usize) -> Vec<Line<'static>> {
        let start = start.min(self.row_count());
        let end = end.min(self.row_count()).max(start);
        (start..end)
            .map(|row| {
                let row_start = self.row_boundaries[row];
                let row_end = self.row_boundaries[row + 1];
                let mut spans = Vec::new();
                if row_start < row_end {
                    let mut run_index = self
                        .style_runs
                        .partition_point(|run| run.start <= row_start)
                        .saturating_sub(1);
                    while let Some(run) = self.style_runs.get(run_index) {
                        let next_start = self
                            .style_runs
                            .get(run_index + 1)
                            .map(|next| next.start)
                            .unwrap_or(self.text.len());
                        let segment_start = row_start.max(run.start);
                        let segment_end = row_end.min(next_start);
                        if segment_start < segment_end {
                            spans.push(Span::styled(
                                self.text[segment_start..segment_end].to_owned(),
                                run.style,
                            ));
                        }
                        if next_start >= row_end {
                            break;
                        }
                        run_index += 1;
                    }
                }
                let mut line = Line::from(spans);
                line.alignment = self.alignment;
                line
            })
            .collect()
    }
}

fn wrap_line_ratatui_compatible(line: &Line<'_>, width: u16) -> CompactWrappedLine {
    let mut wrapped = CompactWrappedLine::new(line.alignment);
    if width == 0 {
        return wrapped;
    }

    // This mirrors ratatui 0.29's WordWrapper with trim=false. Paragraph's
    // scroll offset is u16, so exceptionally tall logical lines get a compact
    // row index and only visible rows are materialized as ratatui Lines.
    let mut pending_line: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut pending_word: Vec<StyledGrapheme<'_>> = Vec::new();
    let mut pending_whitespace: VecDeque<StyledGrapheme<'_>> = VecDeque::new();
    let mut line_width = 0u16;
    let mut word_width = 0u16;
    let mut whitespace_width = 0u16;
    let mut non_whitespace_previous = false;

    for grapheme in line.styled_graphemes(Style::default()) {
        let is_whitespace = grapheme_is_whitespace(&grapheme);
        let symbol_width = grapheme.symbol.width() as u16;
        if symbol_width > width {
            continue;
        }

        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = pending_line.is_empty()
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;

        if word_found || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width = line_width.saturating_add(whitespace_width);
            pending_line.append(&mut pending_word);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;

        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            wrapped.push_row(mem::take(&mut pending_line));
            line_width = 0;

            while let Some(grapheme) = pending_whitespace.front() {
                let grapheme_width = grapheme.symbol.width() as u16;
                if grapheme_width > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(grapheme_width);
                remaining_width = remaining_width.saturating_sub(grapheme_width);
                if let Some(dropped) = pending_whitespace.pop_front() {
                    wrapped.note_gap(dropped.symbol);
                }
            }

            if is_whitespace && pending_whitespace.is_empty() {
                wrapped.note_gap(grapheme.symbol);
                non_whitespace_previous = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(grapheme);
        } else {
            word_width = word_width.saturating_add(symbol_width);
            pending_word.push(grapheme);
        }
        non_whitespace_previous = !is_whitespace;
    }

    if pending_line.is_empty() && pending_word.is_empty() && !pending_whitespace.is_empty() {
        wrapped.push_row(Vec::new());
    }
    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        wrapped.push_row(pending_line);
    }
    if wrapped.row_count() == 0 {
        wrapped.push_row(Vec::new());
    }

    wrapped
}

fn grapheme_is_whitespace(grapheme: &StyledGrapheme<'_>) -> bool {
    const NBSP: &str = "\u{00a0}";
    const ZWSP: &str = "\u{200b}";

    grapheme.symbol == ZWSP
        || (grapheme.symbol.chars().all(char::is_whitespace) && grapheme.symbol != NBSP)
}

pub(crate) fn viewport_paragraph(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThemeIdentity {
    border: Color,
    text: Color,
    muted: Color,
    user: Color,
    success: Color,
    warning: Color,
    error: Color,
    approval: Color,
    diff_add: Color,
    diff_remove: Color,
    markdown_h1: Color,
    markdown_h2: Color,
    markdown_h3: Color,
    markdown_inline_code: Color,
    color_level: TerminalColorLevel,
}

impl From<&Theme> for ThemeIdentity {
    fn from(theme: &Theme) -> Self {
        Self {
            border: theme.border,
            text: theme.text,
            muted: theme.muted,
            user: theme.user,
            success: theme.success,
            warning: theme.warning,
            error: theme.error,
            approval: theme.approval,
            diff_add: theme.diff_add,
            diff_remove: theme.diff_remove,
            markdown_h1: theme.markdown_h1,
            markdown_h2: theme.markdown_h2,
            markdown_h3: theme.markdown_h3,
            markdown_inline_code: theme.markdown_inline_code,
            color_level: theme.color_level,
        }
    }
}

#[derive(Clone, Debug)]
struct CachedMessage {
    search_generation: u64,
    revision: u64,
    width: usize,
    theme: ThemeIdentity,
    syntax_theme_revision: u64,
    force_expand: bool,
    spinner_phase: Option<u8>,
    wrapped_lines: Vec<CompactWrappedLine>,
    line_cumulative_heights: Vec<usize>,
    visual_height: usize,
}

struct SearchableLogicalLine {
    text: String,
    boundaries: Vec<(usize, SelectionPos)>,
}

#[derive(Clone, Debug)]
struct CachedSearchEntry {
    generation: u64,
    matches: Vec<TranscriptSearchMatch>,
}

#[derive(Debug, Default)]
struct TranscriptSearchIndex {
    query: Option<SearchQuery>,
    entries: Vec<Option<CachedSearchEntry>>,
}

#[derive(Clone, Copy, Debug)]
struct ReflowWindow {
    first_retained_message: usize,
    requested_scroll: usize,
    visible_height: usize,
}

#[derive(Clone, Copy, Debug)]
struct ReflowAnchor {
    message_index: usize,
    row_within_message: usize,
}

#[derive(Debug)]
struct ReflowSchedule {
    generation: u64,
    pending_indices: BTreeSet<usize>,
    anchor: Option<ReflowAnchor>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TranscriptPrepareOutcome {
    pub(crate) adjusted_scroll: Option<usize>,
}

impl CachedMessage {
    fn matches(
        &self,
        revision: u64,
        width: usize,
        theme: ThemeIdentity,
        syntax_theme_revision: u64,
        force_expand: bool,
        spinner_phase: Option<u8>,
    ) -> bool {
        self.revision == revision
            && self.width == width
            && self.theme == theme
            && self.syntax_theme_revision == syntax_theme_revision
            && self.force_expand == force_expand
            && self.spinner_phase == spinner_phase
    }

    fn patch_spinner(
        &mut self,
        revision: u64,
        width: usize,
        theme: ThemeIdentity,
        syntax_theme_revision: u64,
        force_expand: bool,
        spinner_phase: Option<u8>,
    ) -> bool {
        let Some(spinner_phase) = spinner_phase else {
            return false;
        };
        if self.revision != revision
            || self.width != width
            || self.theme != theme
            || self.syntax_theme_revision != syntax_theme_revision
            || self.force_expand != force_expand
            || self.spinner_phase.is_none()
            || self.spinner_phase == Some(spinner_phase)
        {
            return false;
        }

        let Some(content) = self.wrapped_lines.first_mut().map(|line| &mut line.text) else {
            return false;
        };
        let Some(old_icon) = content.get(2..).and_then(|rest| rest.chars().next()) else {
            return false;
        };
        let icon_end = 2 + old_icon.len_utf8();
        if content.get(..2) != Some("  ")
            || !SPINNER_FRAMES.contains(&content.get(2..icon_end).unwrap_or_default())
        {
            return false;
        }
        content.replace_range(
            2..icon_end,
            SPINNER_FRAMES[spinner_phase as usize % SPINNER_FRAMES.len()],
        );
        self.spinner_phase = Some(spinner_phase);
        true
    }
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptRenderCache {
    entries: Vec<Option<CachedMessage>>,
    cumulative_heights: Vec<usize>,
    dirty_indices: BTreeSet<usize>,
    spinner_indices: BTreeSet<usize>,
    prepared_width: Option<usize>,
    prepared_theme: Option<ThemeIdentity>,
    prepared_syntax_theme_revision: Option<u64>,
    prepared_force_expand: Option<bool>,
    prepared_spinner_phase: Option<u8>,
    content_generation: u64,
    next_search_generation: u64,
    search_index: RefCell<TranscriptSearchIndex>,
    reflow_generation: u64,
    reflow_schedule: Option<ReflowSchedule>,
    #[cfg(test)]
    last_prepare_visited: usize,
    #[cfg(test)]
    last_search_lines_visited: Cell<usize>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TranscriptRenderContext<'a> {
    width: usize,
    theme: &'a Theme,
    syntax_theme_revision: u64,
    tick: u64,
    force_expand: bool,
    reflow_window: Option<ReflowWindow>,
    reflow_entry_budget: Option<usize>,
}

impl<'a> TranscriptRenderContext<'a> {
    pub(crate) fn new(theme: &'a Theme, width: usize, tick: u64, force_expand: bool) -> Self {
        Self {
            width,
            theme,
            syntax_theme_revision: theme.syntax_theme_revision,
            tick,
            force_expand,
            reflow_window: None,
            reflow_entry_budget: None,
        }
    }

    pub(crate) fn with_reflow_window(
        mut self,
        first_retained_message: usize,
        requested_scroll: usize,
        visible_height: usize,
    ) -> Self {
        self.reflow_window = Some(ReflowWindow {
            first_retained_message,
            requested_scroll,
            visible_height,
        });
        self
    }

    #[cfg(test)]
    fn with_reflow_entry_budget(mut self, entry_budget: usize) -> Self {
        self.reflow_entry_budget = Some(entry_budget);
        self
    }

    #[cfg(test)]
    fn with_syntax_theme_revision(mut self, syntax_theme_revision: u64) -> Self {
        self.syntax_theme_revision = syntax_theme_revision;
        self
    }
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptViewport {
    pub lines: Vec<Line<'static>>,
    pub total_height: usize,
    pub scroll_offset: usize,
    /// Absolute visual row (cache space) of the first rendered line; anchors
    /// screen coordinates to content space for mouse selection.
    pub base_row: usize,
    #[cfg(test)]
    pub first_message: usize,
    #[cfg(test)]
    pub last_message: usize,
    #[cfg(test)]
    pub rendered_message_count: usize,
}

impl TranscriptRenderCache {
    pub(crate) fn message_row_range(&self, message_index: usize) -> Option<std::ops::Range<usize>> {
        let start = *self.cumulative_heights.get(message_index)?;
        let end = *self
            .cumulative_heights
            .get(message_index.saturating_add(1))?;
        Some(start..end)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn populated_len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    #[cfg(test)]
    fn oversized_storage_segments(&self, message_index: usize, line_index: usize) -> usize {
        self.entries
            .get(message_index)
            .and_then(Option::as_ref)
            .and_then(|entry| entry.wrapped_lines.get(line_index))
            .map(|line| line.style_runs.len())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn last_prepare_visited(&self) -> usize {
        self.last_prepare_visited
    }

    #[cfg(test)]
    pub(crate) fn last_search_lines_visited(&self) -> usize {
        self.last_search_lines_visited.get()
    }

    pub fn clear(&mut self) {
        let next_generation = next_generation(self.content_generation);
        *self = Self {
            content_generation: next_generation,
            ..Self::default()
        };
    }

    pub(crate) fn content_generation(&self) -> u64 {
        self.content_generation
    }

    fn bump_content_generation(&mut self) {
        self.content_generation = next_generation(self.content_generation);
    }

    pub(crate) fn search(
        &self,
        first_retained_message: usize,
        query: &SearchQuery,
    ) -> Vec<TranscriptSearchMatch> {
        #[cfg(test)]
        self.last_search_lines_visited.set(0);
        if query.is_empty() {
            return Vec::new();
        }
        let mut search_index = self.search_index.borrow_mut();
        if search_index.query.as_ref() != Some(query) {
            search_index.query = Some(query.clone());
            search_index.entries.clear();
        }
        search_index
            .entries
            .resize_with(self.entries.len(), || None);

        let mut matches = Vec::new();
        for message_index in first_retained_message.min(self.entries.len())..self.entries.len() {
            let Some(cached) = self.entries[message_index].as_ref() else {
                search_index.entries[message_index] = None;
                continue;
            };
            let needs_scan = search_index.entries[message_index]
                .as_ref()
                .is_none_or(|entry| entry.generation != cached.search_generation);
            if needs_scan {
                let mut entry_matches = Vec::new();
                for (line_index, wrapped) in cached.wrapped_lines.iter().enumerate() {
                    #[cfg(test)]
                    self.last_search_lines_visited
                        .set(self.last_search_lines_visited.get() + 1);
                    let line_start = cached
                        .line_cumulative_heights
                        .get(line_index)
                        .copied()
                        .unwrap_or_default();
                    let searchable = searchable_logical_line(
                        wrapped,
                        line_start,
                        line_index == 0 && cached.spinner_phase.is_some(),
                    );
                    for byte_range in query.find_ranges(&searchable.text) {
                        let Ok(start_index) = searchable
                            .boundaries
                            .binary_search_by_key(&byte_range.start, |(byte, _)| *byte)
                        else {
                            continue;
                        };
                        let Ok(end_index) = searchable
                            .boundaries
                            .binary_search_by_key(&byte_range.end, |(byte, _)| *byte)
                        else {
                            continue;
                        };
                        entry_matches.push(TranscriptSearchMatch::new(
                            searchable.boundaries[start_index].1,
                            searchable.boundaries[end_index].1,
                            TranscriptLineIdentity {
                                message_revision: cached.revision,
                                line_index,
                            },
                            byte_range,
                        ));
                    }
                }
                search_index.entries[message_index] = Some(CachedSearchEntry {
                    generation: cached.search_generation,
                    matches: entry_matches,
                });
            }

            let message_start = self
                .cumulative_heights
                .get(message_index)
                .copied()
                .unwrap_or_default();
            if let Some(entry) = search_index.entries[message_index].as_ref() {
                for found in &entry.matches {
                    let mut found = found.clone();
                    found.start.row = found.start.row.saturating_add(message_start);
                    found.end.row = found.end.row.saturating_add(message_start);
                    matches.push(found);
                }
            }
        }
        matches
    }

    pub(crate) fn reveal_offset(
        &self,
        first_retained_message: usize,
        current_offset: usize,
        visible_height: usize,
        found: &TranscriptSearchMatch,
    ) -> usize {
        let live_start = first_retained_message.min(self.entries.len());
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let absolute_total = self.cumulative_heights.last().copied().unwrap_or_default();
        let total_height = absolute_total.saturating_sub(base_height);
        let max_scroll = total_height.saturating_sub(visible_height);
        let current = current_offset.min(max_scroll);
        if visible_height == 0 {
            return current;
        }

        let visible_start = base_height.saturating_add(current);
        let visible_end = visible_start.saturating_add(visible_height);
        let match_start = found.start.row;
        let match_end = found.last_covered_row();
        let target = if match_start < visible_start {
            match_start.saturating_sub(base_height)
        } else if match_end >= visible_end {
            match_end
                .saturating_add(1)
                .saturating_sub(visible_height)
                .saturating_sub(base_height)
        } else {
            current
        };
        target.min(max_scroll)
    }

    pub fn reconcile_len(&mut self, len: usize) {
        if self.entries.len() == len {
            return;
        }
        if len < self.entries.len() {
            self.entries.truncate(len);
            self.dirty_indices.retain(|index| *index < len);
            self.spinner_indices.retain(|index| *index < len);
            self.reset_reflow_after_structural_change();
            self.rebuild_cumulative_heights();
            return;
        }
        if self.cumulative_heights.is_empty() {
            self.cumulative_heights.push(0);
        }
        while self.entries.len() < len {
            let index = self.entries.len();
            self.entries.push(None);
            self.dirty_indices.insert(index);
            self.cumulative_heights
                .push(self.cumulative_heights.last().copied().unwrap_or_default());
        }
    }

    pub fn truncate(&mut self, len: usize) {
        let changed = len < self.entries.len();
        self.entries.truncate(len);
        self.dirty_indices.retain(|index| *index < len);
        self.spinner_indices.retain(|index| *index < len);
        if changed {
            self.reset_reflow_after_structural_change();
        }
        self.rebuild_cumulative_heights();
        if changed {
            self.bump_content_generation();
        }
    }

    pub fn retain(&mut self, keep: &[bool]) {
        let original_len = self.entries.len();
        let old_entries = mem::take(&mut self.entries);
        let old_dirty = mem::take(&mut self.dirty_indices);
        let old_spinners = mem::take(&mut self.spinner_indices);
        self.entries.reserve(old_entries.len().min(keep.len()));

        for (old_index, entry) in old_entries.into_iter().enumerate() {
            if !keep.get(old_index).copied().unwrap_or(false) {
                continue;
            }
            let new_index = self.entries.len();
            if old_dirty.contains(&old_index) || entry.is_none() {
                self.dirty_indices.insert(new_index);
            }
            if old_spinners.contains(&old_index) {
                self.spinner_indices.insert(new_index);
            }
            self.entries.push(entry);
        }
        if self.entries.len() != original_len {
            self.reset_reflow_after_structural_change();
        }
        self.rebuild_cumulative_heights();
        if self.entries.len() != original_len {
            self.bump_content_generation();
        }
    }

    pub fn invalidate(&mut self, index: usize) {
        if index < self.entries.len() {
            self.dirty_indices.insert(index);
        }
    }

    pub fn prepare<F>(
        &mut self,
        messages: &[ChatMessage],
        revisions: &[u64],
        context: TranscriptRenderContext<'_>,
        mut build_message: F,
    ) -> TranscriptPrepareOutcome
    where
        F: FnMut(usize, &ChatMessage, &Theme, usize, u64, bool) -> Vec<Line<'static>>,
    {
        let TranscriptRenderContext {
            width,
            theme,
            syntax_theme_revision,
            tick,
            force_expand,
            reflow_window,
            reflow_entry_budget,
        } = context;
        let width = width.max(1);
        let theme_identity = ThemeIdentity::from(theme);
        self.reconcile_len(messages.len());
        let layout_changed = self.prepared_width != Some(width)
            || self.prepared_theme != Some(theme_identity)
            || self.prepared_syntax_theme_revision != Some(syntax_theme_revision)
            || self.prepared_force_expand != Some(force_expand);
        if layout_changed {
            let can_schedule = self.prepared_width.is_some()
                && self.entries.iter().any(Option::is_some)
                && reflow_window.is_some();
            if can_schedule {
                self.reflow_generation = next_generation(self.reflow_generation);
                self.reflow_schedule = Some(ReflowSchedule {
                    generation: self.reflow_generation,
                    pending_indices: (0..messages.len()).collect(),
                    anchor: reflow_window.and_then(|window| self.reflow_anchor(window)),
                });
            } else {
                self.dirty_indices.extend(0..messages.len());
                self.reflow_schedule = None;
            }
            self.prepared_width = Some(width);
            self.prepared_theme = Some(theme_identity);
            self.prepared_syntax_theme_revision = Some(syntax_theme_revision);
            self.prepared_force_expand = Some(force_expand);
        }
        let spinner_phase = ((tick / 2) % 10) as u8;
        if self.prepared_spinner_phase != Some(spinner_phase) {
            self.dirty_indices
                .extend(self.spinner_indices.iter().copied());
            self.prepared_spinner_phase = Some(spinner_phase);
        }
        let mut immediate_indices = mem::take(&mut self.dirty_indices);
        if let (Some(schedule), Some(window)) = (&self.reflow_schedule, reflow_window) {
            debug_assert_eq!(schedule.generation, self.reflow_generation);
            let visible = self.visible_message_range(window);
            immediate_indices.extend(
                schedule
                    .pending_indices
                    .range(visible.0..visible.1)
                    .copied(),
            );
        }
        let mut first_height_change: Option<usize> = None;
        let mut rebuilt_any = false;
        #[cfg(test)]
        {
            self.last_prepare_visited = 0;
        }

        for index in immediate_indices {
            let (height_changed, rebuilt) = self.prepare_entry(
                index,
                messages,
                revisions,
                theme,
                width,
                syntax_theme_revision,
                tick,
                force_expand,
                theme_identity,
                &mut build_message,
            );
            if height_changed {
                first_height_change = Some(
                    first_height_change
                        .map(|earliest| earliest.min(index))
                        .unwrap_or(index),
                );
            }
            rebuilt_any |= rebuilt;
            if let Some(schedule) = &mut self.reflow_schedule {
                schedule.pending_indices.remove(&index);
            }
        }

        const DEFAULT_REFLOW_ENTRY_BUDGET: usize = 64;
        let reflow_started = Instant::now();
        let reflow_entry_budget = reflow_entry_budget.unwrap_or(DEFAULT_REFLOW_ENTRY_BUDGET);
        let mut budgeted_entries = 0usize;
        loop {
            if budgeted_entries >= reflow_entry_budget
                || (budgeted_entries > 0 && reflow_started.elapsed() >= Duration::from_millis(5))
            {
                break;
            }
            let Some(index) = self
                .reflow_schedule
                .as_ref()
                .and_then(|schedule| schedule.pending_indices.first().copied())
            else {
                break;
            };
            let (height_changed, rebuilt) = self.prepare_entry(
                index,
                messages,
                revisions,
                theme,
                width,
                syntax_theme_revision,
                tick,
                force_expand,
                theme_identity,
                &mut build_message,
            );
            if height_changed {
                first_height_change = Some(
                    first_height_change
                        .map(|earliest| earliest.min(index))
                        .unwrap_or(index),
                );
            }
            rebuilt_any |= rebuilt;
            budgeted_entries += 1;
            if let Some(schedule) = &mut self.reflow_schedule {
                schedule.pending_indices.remove(&index);
            }
        }

        if self.cumulative_heights.len() != self.entries.len() + 1 {
            first_height_change = Some(0);
        }
        if let Some(index) = first_height_change {
            self.rebuild_cumulative_heights_from(index);
        }
        if rebuilt_any {
            self.bump_content_generation();
        }
        let adjusted_scroll = self
            .reflow_schedule
            .as_ref()
            .and_then(|schedule| schedule.anchor)
            .and_then(|anchor| reflow_window.map(|window| self.scroll_for_anchor(window, anchor)));
        if self
            .reflow_schedule
            .as_ref()
            .is_some_and(|schedule| schedule.pending_indices.is_empty())
        {
            self.reflow_schedule = None;
        }
        TranscriptPrepareOutcome { adjusted_scroll }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_entry<F>(
        &mut self,
        index: usize,
        messages: &[ChatMessage],
        revisions: &[u64],
        theme: &Theme,
        width: usize,
        syntax_theme_revision: u64,
        tick: u64,
        force_expand: bool,
        theme_identity: ThemeIdentity,
        build_message: &mut F,
    ) -> (bool, bool)
    where
        F: FnMut(usize, &ChatMessage, &Theme, usize, u64, bool) -> Vec<Line<'static>>,
    {
        let Some(message) = messages.get(index) else {
            return (false, false);
        };
        #[cfg(test)]
        {
            self.last_prepare_visited += 1;
        }
        let revision = revisions.get(index).copied().unwrap_or_default();
        let spinner_phase = message_spinner_phase(message, tick);
        if self.entries[index].as_mut().is_some_and(|cached| {
            cached.patch_spinner(
                revision,
                width,
                theme_identity,
                syntax_theme_revision,
                force_expand,
                spinner_phase,
            )
        }) {
            return (false, false);
        }
        if self.entries[index].as_ref().is_some_and(|cached| {
            cached.matches(
                revision,
                width,
                theme_identity,
                syntax_theme_revision,
                force_expand,
                spinner_phase,
            )
        }) {
            return (false, false);
        }

        let lines = build_message(index, message, theme, width, tick, force_expand);
        let ratatui_width = width.min(u16::MAX as usize) as u16;
        let wrapped_lines = lines
            .iter()
            .map(|line| wrap_line_ratatui_compatible(line, ratatui_width))
            .collect::<Vec<_>>();
        let mut line_cumulative_heights: Vec<usize> = Vec::with_capacity(wrapped_lines.len() + 1);
        line_cumulative_heights.push(0);
        for line in &wrapped_lines {
            let next = line_cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(line.row_count());
            line_cumulative_heights.push(next);
        }
        let visual_height = line_cumulative_heights.last().copied().unwrap_or_default();
        if spinner_phase.is_some() {
            self.spinner_indices.insert(index);
        } else {
            self.spinner_indices.remove(&index);
        }
        let height_changed = self.entries[index]
            .as_ref()
            .is_none_or(|cached| cached.visual_height != visual_height);
        self.next_search_generation = next_generation(self.next_search_generation);
        self.entries[index] = Some(CachedMessage {
            search_generation: self.next_search_generation,
            revision,
            width,
            theme: theme_identity,
            syntax_theme_revision,
            force_expand,
            spinner_phase,
            wrapped_lines,
            line_cumulative_heights,
            visual_height,
        });
        (height_changed, true)
    }

    fn visible_message_range(&self, window: ReflowWindow) -> (usize, usize) {
        let message_count = self.entries.len();
        let live_start = window.first_retained_message.min(message_count);
        if live_start == message_count || window.visible_height == 0 {
            return (live_start, live_start);
        }
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let absolute_total = self.cumulative_heights.last().copied().unwrap_or_default();
        let total_height = absolute_total.saturating_sub(base_height);
        let scroll_offset = window
            .requested_scroll
            .min(total_height.saturating_sub(window.visible_height));
        let absolute_scroll = base_height.saturating_add(scroll_offset);
        let absolute_end = absolute_scroll
            .saturating_add(window.visible_height)
            .min(absolute_total);
        let first_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height <= absolute_scroll)
            .saturating_sub(1)
            .max(live_start)
            .min(message_count - 1);
        let last_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height < absolute_end)
            .max(first_visible + 1)
            .min(message_count);
        (first_visible, last_visible)
    }

    fn reflow_anchor(&self, window: ReflowWindow) -> Option<ReflowAnchor> {
        if window.requested_scroll == usize::MAX {
            return None;
        }
        let (message_index, _) = self.visible_message_range(window);
        let cached = self.entries.get(message_index)?.as_ref()?;
        let live_start = window.first_retained_message.min(self.entries.len());
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let absolute_total = self.cumulative_heights.last().copied().unwrap_or_default();
        let total_height = absolute_total.saturating_sub(base_height);
        let scroll_offset = window
            .requested_scroll
            .min(total_height.saturating_sub(window.visible_height));
        let absolute_scroll = base_height.saturating_add(scroll_offset);
        Some(ReflowAnchor {
            message_index,
            row_within_message: absolute_scroll
                .saturating_sub(self.cumulative_heights[message_index])
                .min(cached.visual_height.saturating_sub(1)),
        })
    }

    fn scroll_for_anchor(&self, window: ReflowWindow, anchor: ReflowAnchor) -> usize {
        let live_start = window.first_retained_message.min(self.entries.len());
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let message_start = self
            .cumulative_heights
            .get(anchor.message_index)
            .copied()
            .unwrap_or(base_height);
        let row_within = self
            .entries
            .get(anchor.message_index)
            .and_then(Option::as_ref)
            .map(|cached| {
                anchor
                    .row_within_message
                    .min(cached.visual_height.saturating_sub(1))
            })
            .unwrap_or_default();
        message_start
            .saturating_add(row_within)
            .saturating_sub(base_height)
    }

    #[cfg(test)]
    fn reflow_pending_for_test(&self) -> bool {
        self.reflow_schedule
            .as_ref()
            .is_some_and(|schedule| !schedule.pending_indices.is_empty())
    }

    fn reset_reflow_after_structural_change(&mut self) {
        if let Some(schedule) = &mut self.reflow_schedule {
            schedule.pending_indices = (0..self.entries.len()).collect();
            schedule.anchor = None;
        }
    }

    pub fn viewport(
        &self,
        first_retained_message: usize,
        requested_scroll: usize,
        visible_height: usize,
    ) -> TranscriptViewport {
        let message_count = self.entries.len();
        let live_start = first_retained_message.min(message_count);
        let base_height = self
            .cumulative_heights
            .get(live_start)
            .copied()
            .unwrap_or_default();
        let absolute_total = self.cumulative_heights.last().copied().unwrap_or_default();
        let total_height = absolute_total.saturating_sub(base_height);
        let max_scroll = total_height.saturating_sub(visible_height);
        let scroll_offset = requested_scroll.min(max_scroll);
        if live_start == message_count || visible_height == 0 {
            return TranscriptViewport {
                total_height,
                scroll_offset,
                base_row: base_height.saturating_add(scroll_offset),
                #[cfg(test)]
                first_message: live_start,
                #[cfg(test)]
                last_message: live_start,
                ..TranscriptViewport::default()
            };
        }

        let absolute_scroll = base_height.saturating_add(scroll_offset);
        let absolute_end = absolute_scroll
            .saturating_add(visible_height)
            .min(absolute_total);
        let first_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height <= absolute_scroll)
            .saturating_sub(1)
            .max(live_start)
            .min(message_count - 1);
        let last_visible = self.cumulative_heights[..message_count]
            .partition_point(|height| *height < absolute_end)
            .max(first_visible + 1)
            .min(message_count);

        let first_message = first_visible;
        let last_message = last_visible;
        let mut lines = Vec::new();
        for message_index in first_message..last_message {
            let Some(cached) = self.entries[message_index].as_ref() else {
                continue;
            };
            if cached.wrapped_lines.is_empty() {
                continue;
            }

            let message_start = self
                .cumulative_heights
                .get(message_index)
                .copied()
                .unwrap_or_default();
            let relative_start = absolute_scroll
                .saturating_sub(message_start)
                .min(cached.visual_height);
            let relative_end = absolute_end
                .saturating_sub(message_start)
                .min(cached.visual_height);
            if relative_start >= relative_end {
                continue;
            }

            let line_count = cached.wrapped_lines.len();
            let first_line = cached.line_cumulative_heights[..line_count]
                .partition_point(|height| *height <= relative_start)
                .saturating_sub(1)
                .min(line_count - 1);
            let last_line = cached.line_cumulative_heights[..line_count]
                .partition_point(|height| *height < relative_end)
                .max(first_line + 1)
                .min(line_count);

            for line_index in first_line..last_line {
                let line_start = cached.line_cumulative_heights[line_index];
                let line_end = cached.line_cumulative_heights[line_index + 1];
                let line_height = line_end.saturating_sub(line_start);
                let visual_start = relative_start.saturating_sub(line_start).min(line_height);
                let visual_end = relative_end.saturating_sub(line_start).min(line_height);
                if visual_start >= visual_end {
                    continue;
                }

                let visual_line = &cached.wrapped_lines[line_index];
                let visual_start = visual_start.min(visual_line.row_count());
                let visual_end = visual_end.min(visual_line.row_count());
                lines.extend(visual_line.materialize_rows(visual_start, visual_end));
            }
        }

        TranscriptViewport {
            lines,
            total_height,
            scroll_offset,
            base_row: absolute_scroll,
            #[cfg(test)]
            first_message,
            #[cfg(test)]
            last_message,
            #[cfg(test)]
            rendered_message_count: last_message.saturating_sub(first_message),
        }
    }

    /// Resolve an absolute visual row to its message, wrapped line, and row
    /// index within that line. `None` past the end of the transcript.
    fn locate_row(&self, row: usize) -> Option<(usize, usize, usize)> {
        let message_count = self.entries.len();
        let total_rows = self.cumulative_heights.last().copied().unwrap_or_default();
        if message_count == 0 || row >= total_rows {
            return None;
        }
        let message_index = self.cumulative_heights[..message_count]
            .partition_point(|height| *height <= row)
            .saturating_sub(1);
        let cached = self.entries.get(message_index).and_then(Option::as_ref)?;
        let relative = row - self.cumulative_heights[message_index];
        let line_count = cached.wrapped_lines.len();
        if line_count == 0 || relative >= cached.visual_height {
            return None;
        }
        let line_index = cached.line_cumulative_heights[..line_count]
            .partition_point(|height| *height <= relative)
            .saturating_sub(1);
        let row_within = relative - cached.line_cumulative_heights[line_index];
        if row_within >= cached.wrapped_lines[line_index].row_count() {
            return None;
        }
        Some((message_index, line_index, row_within))
    }

    fn row_text(&self, message_index: usize, line_index: usize, row_within: usize) -> &str {
        let Some(cached) = self.entries.get(message_index).and_then(Option::as_ref) else {
            return "";
        };
        let Some(wrapped) = cached.wrapped_lines.get(line_index) else {
            return "";
        };
        &wrapped.text[wrapped.row_boundaries[row_within]..wrapped.row_boundaries[row_within + 1]]
    }

    /// Inclusive cell bounds of the same-class run (word) under `pos`. The
    /// run expands across soft-wrap boundaries within the logical line: a
    /// non-empty wrap gap (whitespace the wrapper dropped) separates words,
    /// while an empty gap means a hard-split word (URL) that continues on the
    /// next row. `None` when the click lands past the end of the row or
    /// transcript.
    pub fn word_bounds_at(&self, pos: SelectionPos) -> Option<(SelectionPos, SelectionPos)> {
        use unicode_width::UnicodeWidthChar;

        #[derive(Clone, Copy, PartialEq)]
        enum CharClass {
            Whitespace,
            Word,
            Punctuation,
        }
        fn classify(ch: char) -> CharClass {
            if ch.is_whitespace() {
                CharClass::Whitespace
            } else if ch.is_alphanumeric() || ch == '_' {
                CharClass::Word
            } else {
                CharClass::Punctuation
            }
        }

        let (message_index, line_index, clicked_row_within) = self.locate_row(pos.row)?;
        let cached = self.entries.get(message_index).and_then(Option::as_ref)?;
        let wrapped = cached.wrapped_lines.get(line_index)?;
        let line_first_abs_row = pos.row - clicked_row_within;

        // Cells across the whole logical line, in reading order. `None`
        // position marks whitespace the wrapper dropped at a soft wrap — it
        // separates runs like any other whitespace but cannot be selected.
        let mut cells: Vec<(Option<SelectionPos>, CharClass)> = Vec::new();
        let mut clicked: Option<usize> = None;
        for row_within in 0..wrapped.row_count() {
            let text = self.row_text(message_index, line_index, row_within);
            let mut col = 0usize;
            for ch in text.chars() {
                let width = ch.width().unwrap_or(0);
                if row_within == clicked_row_within
                    && col <= pos.col
                    && pos.col < col + width.max(1)
                {
                    clicked = Some(cells.len());
                }
                cells.push((
                    Some(SelectionPos {
                        row: line_first_abs_row + row_within,
                        col,
                    }),
                    classify(ch),
                ));
                col += width;
            }
            let separates = wrapped
                .wrap_gaps
                .get(row_within)
                .is_some_and(|gap| !gap.is_empty());
            if separates {
                cells.push((None, CharClass::Whitespace));
            }
        }

        let clicked = clicked?;
        let class = cells[clicked].1;
        let mut first = clicked;
        while first > 0 && cells[first - 1].1 == class {
            first -= 1;
        }
        let mut last = clicked;
        while last + 1 < cells.len() && cells[last + 1].1 == class {
            last += 1;
        }
        // Virtual gap cells have no position; shrink to the real ends.
        while first < last && cells[first].0.is_none() {
            first += 1;
        }
        while last > first && cells[last].0.is_none() {
            last -= 1;
        }
        Some((cells[first].0?, cells[last].0?))
    }

    /// Inclusive cell bounds of the whole logical (unwrapped) line under
    /// `pos`: from column 0 of its first visual row to the last character's
    /// leading column on its last visual row.
    pub fn line_bounds_at(&self, pos: SelectionPos) -> Option<(SelectionPos, SelectionPos)> {
        use unicode_width::UnicodeWidthChar;

        let (message_index, line_index, row_within) = self.locate_row(pos.row)?;
        let cached = self.entries.get(message_index).and_then(Option::as_ref)?;
        let wrapped = cached.wrapped_lines.get(line_index)?;
        let first_abs_row = pos.row - row_within;
        let last_row_within = wrapped.row_count().saturating_sub(1);
        let last_text = self.row_text(message_index, line_index, last_row_within);
        let mut last_leading_col = 0usize;
        let mut col = 0usize;
        for ch in last_text.chars() {
            last_leading_col = col;
            col += ch.width().unwrap_or(0);
        }
        Some((
            SelectionPos {
                row: first_abs_row,
                col: 0,
            },
            SelectionPos {
                row: first_abs_row + last_row_within,
                col: last_leading_col,
            },
        ))
    }

    /// Extract the selected text for clipboard copy.
    ///
    /// Rows that belong to the same logical (wrapped) line are joined with the
    /// whitespace the word-wrapper dropped at that boundary (`wrap_gaps`), so
    /// soft-wrapped prose reads naturally while a hard-split long word (URL)
    /// stays unbroken. Hard line breaks emit `\n` with trailing whitespace
    /// trimmed, matching what terminals do on copy.
    pub fn extract_text(&self, selection: &TranscriptSelection) -> String {
        let (start, end) = selection.normalized();
        let message_count = self.entries.len();
        let total_rows = self.cumulative_heights.last().copied().unwrap_or_default();
        if message_count == 0 || total_rows == 0 || start.row >= total_rows {
            return String::new();
        }

        let mut out = String::new();
        let mut current_line = String::new();
        let mut previous: Option<(usize, usize, usize)> = None;
        for row in start.row..=end.row.min(total_rows - 1) {
            let Some((message_index, line_index, row_within)) = self.locate_row(row) else {
                continue;
            };

            if let Some((prev_message, prev_line, prev_row)) = previous {
                let soft_wrap = prev_message == message_index
                    && prev_line == line_index
                    && prev_row + 1 == row_within;
                if soft_wrap {
                    let gap = self.entries[message_index]
                        .as_ref()
                        .and_then(|cached| cached.wrapped_lines.get(line_index))
                        .and_then(|wrapped| wrapped.wrap_gaps.get(prev_row));
                    if let Some(gap) = gap {
                        current_line.push_str(gap);
                    }
                } else {
                    let settled = current_line.trim_end().len();
                    current_line.truncate(settled);
                    out.push_str(&current_line);
                    out.push('\n');
                    current_line.clear();
                }
            }

            let (col_start, col_end) = selection.cols_on_row(row).unwrap_or((0, None));
            current_line.push_str(slice_row_by_columns(
                self.row_text(message_index, line_index, row_within),
                col_start,
                col_end,
            ));
            previous = Some((message_index, line_index, row_within));
        }
        let settled = current_line.trim_end().len();
        current_line.truncate(settled);
        out.push_str(&current_line);
        out
    }

    fn rebuild_cumulative_heights(&mut self) {
        self.cumulative_heights.clear();
        self.cumulative_heights.reserve(self.entries.len() + 1);
        self.cumulative_heights.push(0);
        for entry in &self.entries {
            let height = entry
                .as_ref()
                .map(|cached| cached.visual_height)
                .unwrap_or_default();
            let next = self
                .cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(height);
            self.cumulative_heights.push(next);
        }
    }

    fn rebuild_cumulative_heights_from(&mut self, start: usize) {
        let start = start.min(self.entries.len());
        if self.cumulative_heights.len() < start + 1 {
            self.rebuild_cumulative_heights();
            return;
        }
        self.cumulative_heights.truncate(start + 1);
        for entry in &self.entries[start..] {
            let height = entry
                .as_ref()
                .map(|cached| cached.visual_height)
                .unwrap_or_default();
            let next = self
                .cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(height);
            self.cumulative_heights.push(next);
        }
    }
}

fn message_spinner_phase(message: &ChatMessage, tick: u64) -> Option<u8> {
    match message {
        ChatMessage::ToolCall { status, .. }
            if matches!(status.as_str(), "running" | "receiving") =>
        {
            Some(((tick / 2) % 10) as u8)
        }
        _ => None,
    }
}

fn next_generation(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}

fn searchable_logical_line(
    wrapped: &CompactWrappedLine,
    absolute_first_row: usize,
    exclude_spinner: bool,
) -> SearchableLogicalLine {
    use unicode_width::UnicodeWidthChar;

    let mut text = String::new();
    let mut boundaries = Vec::new();
    for row_within in 0..wrapped.row_count() {
        let row_start = wrapped.row_boundaries[row_within];
        let row_end = wrapped.row_boundaries[row_within + 1];
        let row = &wrapped.text[row_start..row_end];
        let absolute_row = absolute_first_row.saturating_add(row_within);
        let mut col = 0usize;
        for (character_index, character) in row.chars().enumerate() {
            let spinner = exclude_spinner
                && row_within == 0
                && character_index == 2
                && SPINNER_FRAMES
                    .iter()
                    .any(|frame| frame.starts_with(character));
            if spinner {
                col = col.saturating_add(character.width().unwrap_or(0));
                continue;
            }
            boundaries.push((
                text.len(),
                SelectionPos {
                    row: absolute_row,
                    col,
                },
            ));
            text.push(character);
            col = col.saturating_add(character.width().unwrap_or(0));
        }
        if let Some(gap) = wrapped.wrap_gaps.get(row_within) {
            let next_row = absolute_row.saturating_add(1);
            let mut gap_col = 0usize;
            for character in gap.chars() {
                boundaries.push((
                    text.len(),
                    SelectionPos {
                        row: next_row,
                        col: gap_col,
                    },
                ));
                text.push(character);
                gap_col = gap_col.saturating_add(character.width().unwrap_or(0));
            }
        }
    }
    let end = if let Some(last_row) = wrapped.row_count().checked_sub(1) {
        let row_start = wrapped.row_boundaries[last_row];
        let row_end = wrapped.row_boundaries[last_row + 1];
        let row = &wrapped.text[row_start..row_end];
        SelectionPos {
            row: absolute_first_row.saturating_add(last_row),
            col: UnicodeWidthStr::width(row),
        }
    } else {
        SelectionPos {
            row: absolute_first_row,
            col: 0,
        }
    };
    boundaries.push((text.len(), end));
    SearchableLogicalLine { text, boundaries }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use ratatui::buffer::Buffer;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    use unicode_width::UnicodeWidthStr;

    use super::{
        SPINNER_FRAMES, TranscriptRenderCache, TranscriptRenderContext, viewport_paragraph,
        wrap_line_ratatui_compatible,
    };
    use crate::selection::{SelectionGranularity, SelectionPos, TranscriptSelection};
    use crate::theme::Theme;
    use crate::transcript_search::{
        SearchQuery, TranscriptLineIdentity, TranscriptSearchMatch, TranscriptSearchState,
    };
    use crate::types::{AppState, ChatMessage, TuiEvent};
    use crate::ui::build_lines_for_messages;

    fn theme() -> Theme {
        Theme::named(orca_core::config::ThemeName::Dark)
    }

    fn render_lines(lines: Vec<Line<'static>>, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);
        buffer
    }

    #[test]
    fn compact_wrapper_matches_ratatui_cells_for_unicode_whitespace_and_styles() {
        let line = Line::from(vec![
            Span::styled("alpha  ", Style::default().fg(Color::Red)),
            Span::styled(
                "世界\u{00a0}wide\u{200b}word ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("tail-tail-tail", Style::default().fg(Color::Blue)),
        ])
        .alignment(Alignment::Center);
        let width = 9;
        let paragraph = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
        let height = paragraph.line_count(width) as u16;
        let expected = render_lines(vec![line.clone()], width, height);

        let compact = wrap_line_ratatui_compatible(&line, width);
        assert_eq!(compact.row_count(), height as usize);
        let actual = render_lines(
            compact.materialize_rows(0, compact.row_count()),
            width,
            height,
        );

        assert_eq!(actual, expected);
    }

    #[test]
    fn whitespace_heavy_tail_matches_original_paragraph_without_double_wrapping() {
        let source_lines = vec![Line::from(" ".repeat(2_050)), Line::from("tail")];
        let width = 1;
        let visible_height = 10usize;
        let source = Paragraph::new(source_lines.clone()).wrap(Wrap { trim: false });
        let total_height = source.line_count(width);
        let mut expected = Buffer::empty(Rect::new(0, 0, width, visible_height as u16));
        source
            .scroll(((total_height - visible_height) as u16, 0))
            .render(expected.area, &mut expected);

        let messages = vec![ChatMessage::System("whitespace".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, width as usize, 0, false),
            |_, _, _, _, _, _| source_lines.clone(),
        );
        let viewport = cache.viewport(0, usize::MAX, visible_height);
        let mut actual = Buffer::empty(expected.area);
        viewport_paragraph(viewport.lines).render(actual.area, &mut actual);

        assert_eq!(actual, expected);
    }

    /// Build a cache whose message `i` renders exactly `lines_per_message[i]`.
    fn extraction_cache(
        lines_per_message: &[Vec<Line<'static>>],
        width: usize,
    ) -> TranscriptRenderCache {
        let messages: Vec<ChatMessage> = (0..lines_per_message.len())
            .map(|index| ChatMessage::System(index.to_string()))
            .collect();
        let revisions: Vec<u64> = (1..=messages.len() as u64).collect();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, width, 0, false),
            |_, message, _, _, _, _| {
                let ChatMessage::System(index) = message else {
                    unreachable!()
                };
                lines_per_message[index.parse::<usize>().unwrap()].clone()
            },
        );
        cache
    }

    fn selection(anchor: (usize, usize), head: (usize, usize)) -> TranscriptSelection {
        let anchor = SelectionPos {
            row: anchor.0,
            col: anchor.1,
        };
        let head = SelectionPos {
            row: head.0,
            col: head.1,
        };
        TranscriptSelection {
            anchor,
            head,
            dragging: false,
            granularity: SelectionGranularity::Cell,
            origin: (anchor, head),
        }
    }

    fn unit_text(cache: &TranscriptRenderCache, bounds: (SelectionPos, SelectionPos)) -> String {
        cache.extract_text(&TranscriptSelection::unit(
            SelectionGranularity::Word,
            bounds.0,
            bounds.1,
        ))
    }

    #[test]
    fn extract_restores_the_space_dropped_at_a_soft_wrap() {
        let cache = extraction_cache(&[vec![Line::from("foo bar baz")]], 5);
        assert_eq!(
            cache.extract_text(&selection((0, 0), (99, 99))),
            "foo bar baz"
        );
    }

    #[test]
    fn extract_does_not_invent_a_gap_inside_a_hard_split_long_word() {
        let cache = extraction_cache(&[vec![Line::from("abcdefghij")]], 4);
        assert_eq!(
            cache.extract_text(&selection((0, 0), (99, 99))),
            "abcdefghij"
        );
    }

    #[test]
    fn extract_emits_newlines_at_hard_line_and_message_boundaries() {
        let cache = extraction_cache(
            &[
                vec![Line::from("alpha")],
                vec![Line::from("beta"), Line::from("gamma")],
            ],
            20,
        );
        assert_eq!(
            cache.extract_text(&selection((0, 0), (99, 99))),
            "alpha\nbeta\ngamma"
        );
    }

    #[test]
    fn extract_respects_column_ranges_on_a_single_row() {
        let cache = extraction_cache(&[vec![Line::from("hello world")]], 20);
        assert_eq!(cache.extract_text(&selection((0, 6), (0, 10))), "world");
        // Backwards drag selects the same range.
        assert_eq!(cache.extract_text(&selection((0, 10), (0, 6))), "world");
    }

    #[test]
    fn extract_counts_display_columns_for_wide_characters() {
        // 你=cols 0-1, 好=2-3, 世=4-5, 界=6-7.
        let cache = extraction_cache(&[vec![Line::from("你好世界")]], 20);
        assert_eq!(cache.extract_text(&selection((0, 2), (0, 4))), "好世");
    }

    #[test]
    fn extract_selects_one_cell_for_same_point_and_nothing_out_of_range() {
        let cache = extraction_cache(&[vec![Line::from("alpha")]], 20);
        // Same-cell endpoints still cover one character ('h' at col 3)...
        assert_eq!(cache.extract_text(&selection((0, 3), (0, 3))), "h");
        // ...but rows past the transcript extract nothing.
        assert_eq!(cache.extract_text(&selection((7, 0), (9, 5))), "");
    }

    #[test]
    fn viewport_base_row_tracks_scroll_position() {
        let cache = extraction_cache(
            &[vec![
                Line::from("one"),
                Line::from("two"),
                Line::from("three"),
                Line::from("four"),
            ]],
            20,
        );
        assert_eq!(cache.viewport(0, 0, 2).base_row, 0);
        assert_eq!(cache.viewport(0, 2, 2).base_row, 2);
        assert_eq!(cache.viewport(0, usize::MAX, 2).base_row, 2);
    }

    #[test]
    fn word_selection_expands_over_same_class_runs() {
        let cache = extraction_cache(&[vec![Line::from("hello world(x)")]], 40);
        // Click inside "world" (cols 6-10).
        let bounds = cache
            .word_bounds_at(SelectionPos { row: 0, col: 8 })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "world");
        // Click on the parenthesis selects just the punctuation run.
        let bounds = cache
            .word_bounds_at(SelectionPos { row: 0, col: 11 })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "(");
        // CJK characters are alphanumeric and select as a run.
        let cache = extraction_cache(&[vec![Line::from("你好 世界")]], 40);
        let bounds = cache
            .word_bounds_at(SelectionPos { row: 0, col: 1 })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "你好");
    }

    #[test]
    fn word_selection_crosses_hard_split_rows_but_not_soft_wrap_gaps() {
        // Width 6 splits the long token across rows with EMPTY gaps: the
        // word continues across the boundary.
        let cache = extraction_cache(&[vec![Line::from("https_example")]], 6);
        let bounds = cache
            .word_bounds_at(SelectionPos { row: 1, col: 2 })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "https_example");

        // "foo bar" at width 4 wraps with a DROPPED SPACE between the rows:
        // the gap separates the words even though the rows are adjacent.
        let cache = extraction_cache(&[vec![Line::from("foo bar")]], 4);
        let bounds = cache
            .word_bounds_at(SelectionPos { row: 1, col: 1 })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "bar");
    }

    #[test]
    fn line_bounds_cover_the_whole_logical_line_across_wrapped_rows() {
        let cache = extraction_cache(
            &[vec![Line::from("alpha beta gamma"), Line::from("tail")]],
            6,
        );
        // Click anywhere inside the wrapped first logical line.
        let bounds = cache
            .line_bounds_at(SelectionPos { row: 1, col: 0 })
            .unwrap();
        assert_eq!(bounds.0, SelectionPos { row: 0, col: 0 });
        assert_eq!(unit_text(&cache, bounds), "alpha beta gamma");
        // The second logical line is its own unit.
        let bounds = cache
            .line_bounds_at(SelectionPos {
                row: bounds.1.row + 1,
                col: 2,
            })
            .unwrap();
        assert_eq!(unit_text(&cache, bounds), "tail");
    }

    #[test]
    fn word_selection_past_the_row_end_is_none() {
        let cache = extraction_cache(&[vec![Line::from("hi")]], 40);
        assert!(
            cache
                .word_bounds_at(SelectionPos { row: 0, col: 10 })
                .is_none()
        );
        assert!(
            cache
                .word_bounds_at(SelectionPos { row: 5, col: 0 })
                .is_none()
        );
    }

    #[derive(Clone, Copy)]
    struct RenderCounters<'a> {
        message_builds: &'a Cell<usize>,
        markdown_parses: &'a Cell<usize>,
    }

    impl<'a> RenderCounters<'a> {
        fn new(message_builds: &'a Cell<usize>, markdown_parses: &'a Cell<usize>) -> Self {
            Self {
                message_builds,
                markdown_parses,
            }
        }
    }

    fn prepare_with_counters(
        cache: &mut TranscriptRenderCache,
        messages: &[ChatMessage],
        revisions: &[u64],
        width: usize,
        syntax_theme_revision: u64,
        tick: u64,
        counters: RenderCounters<'_>,
    ) {
        let theme = theme();
        prepare_with_theme_and_counters(
            cache,
            messages,
            revisions,
            width,
            &theme,
            syntax_theme_revision,
            tick,
            counters,
        );
    }

    fn prepare_with_theme_and_counters(
        cache: &mut TranscriptRenderCache,
        messages: &[ChatMessage],
        revisions: &[u64],
        width: usize,
        theme: &Theme,
        syntax_theme_revision: u64,
        tick: u64,
        counters: RenderCounters<'_>,
    ) {
        cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(theme, width, tick, false)
                .with_syntax_theme_revision(syntax_theme_revision),
            |_, message, theme, width, tick, force_expand| {
                counters
                    .message_builds
                    .set(counters.message_builds.get() + 1);
                if matches!(message, ChatMessage::Assistant(_)) {
                    counters
                        .markdown_parses
                        .set(counters.markdown_parses.get() + 1);
                }
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
    }

    fn prepare_exact(
        cache: &mut TranscriptRenderCache,
        messages: &[ChatMessage],
        revisions: &[u64],
        width: usize,
        tick: u64,
    ) {
        let theme = theme();
        cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, width, tick, false),
            |_, message, theme, width, tick, force_expand| {
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
    }

    #[test]
    fn content_generation_changes_only_when_searchable_cache_content_changes() {
        let messages = vec![ChatMessage::Assistant("alpha".to_string())];
        let mut revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        assert_eq!(cache.content_generation(), 0);

        prepare_exact(&mut cache, &messages, &revisions, 40, 0);
        let built = cache.content_generation();
        assert_ne!(built, 0);

        prepare_exact(&mut cache, &messages, &revisions, 40, 0);
        let _ = cache.viewport(0, 0, 10);
        assert_eq!(cache.content_generation(), built);

        revisions[0] += 1;
        cache.invalidate(0);
        prepare_exact(&mut cache, &messages, &revisions, 40, 0);
        assert_ne!(cache.content_generation(), built);
    }

    #[test]
    fn spinner_only_patch_does_not_change_search_generation() {
        let messages = vec![ChatMessage::ToolCall {
            id: "running".to_string(),
            name: "read".to_string(),
            target: None,
            status: "running".to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        }];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        prepare_exact(&mut cache, &messages, &revisions, 40, 0);
        let generation = cache.content_generation();

        prepare_exact(&mut cache, &messages, &revisions, 40, 2);

        assert_eq!(cache.content_generation(), generation);
    }

    #[test]
    fn structural_cache_mutations_bump_generation_only_when_effective() {
        let messages = vec![
            ChatMessage::System("one".to_string()),
            ChatMessage::System("two".to_string()),
        ];
        let revisions = vec![1, 2];
        let mut cache = TranscriptRenderCache::default();
        prepare_exact(&mut cache, &messages, &revisions, 40, 0);

        let initial = cache.content_generation();
        cache.retain(&[true, true]);
        assert_eq!(cache.content_generation(), initial);

        cache.retain(&[false, true]);
        let retained = cache.content_generation();
        assert_ne!(retained, initial);

        cache.truncate(0);
        let truncated = cache.content_generation();
        assert_ne!(truncated, retained);

        cache.clear();
        assert_ne!(cache.content_generation(), truncated);
    }

    fn prepared_search_cache(
        lines_per_message: &[Vec<Line<'static>>],
        width: usize,
    ) -> TranscriptRenderCache {
        let messages = (0..lines_per_message.len())
            .map(|index| ChatMessage::System(index.to_string()))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, width, 0, false),
            |_, message, _, _, _, _| {
                let ChatMessage::System(index) = message else {
                    unreachable!();
                };
                lines_per_message[index.parse::<usize>().unwrap()].clone()
            },
        );
        cache
    }

    #[test]
    fn search_maps_span_and_soft_wrap_matches_to_absolute_rows() {
        let messages = vec![ChatMessage::System("fixture".to_string())];
        let revisions = vec![7];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 6, 0, false),
            |_, _, _, _, _, _| {
                vec![Line::from(vec![
                    Span::styled("alpha ", Style::default().fg(Color::Red)),
                    Span::styled("beta", Style::default().fg(Color::Blue)),
                ])]
            },
        );

        let matches = cache.search(0, &SearchQuery::new("alpha beta"));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, SelectionPos { row: 0, col: 0 });
        assert_eq!(matches[0].end, SelectionPos { row: 1, col: 4 });
        assert_eq!(matches[0].line_identity.message_revision, 7);
    }

    #[test]
    fn search_does_not_cross_hard_lines_or_message_boundaries() {
        let cache = prepared_search_cache(
            &[
                vec![Line::from("alpha"), Line::from("beta")],
                vec![Line::from("gamma")],
            ],
            80,
        );
        assert!(cache.search(0, &SearchQuery::new("alpha beta")).is_empty());
        assert!(cache.search(0, &SearchQuery::new("beta gamma")).is_empty());
    }

    #[test]
    fn search_maps_cjk_combining_and_emoji_to_display_columns() {
        let cache = prepared_search_cache(&[vec![Line::from("A中e\u{301}👍🏽Z")]], 80);
        let cjk = cache.search(0, &SearchQuery::new("中"));
        assert_eq!((cjk[0].start.col, cjk[0].end.col), (1, 3));
        let combining = cache.search(0, &SearchQuery::new("e\u{301}"));
        assert_eq!((combining[0].start.col, combining[0].end.col), (3, 4));
        let emoji = cache.search(0, &SearchQuery::new("👍🏽"));
        assert!(emoji[0].end.col > emoji[0].start.col);
    }

    #[test]
    fn search_skips_unprepared_and_flushed_entries_and_keeps_repeated_matches() {
        let mut unprepared = TranscriptRenderCache::default();
        unprepared.reconcile_len(1);
        assert!(unprepared.search(0, &SearchQuery::new("x")).is_empty());

        let repeated = prepared_search_cache(&[vec![Line::from("x x x")]], 80);
        assert_eq!(repeated.search(0, &SearchQuery::new("x")).len(), 3);

        let flushed = prepared_search_cache(
            &[
                vec![Line::from("old target")],
                vec![Line::from("live target")],
            ],
            80,
        );
        let found = flushed.search(1, &SearchQuery::new("target"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line_identity.message_revision, 2);
    }

    #[test]
    fn search_coordinates_remain_usize_above_u16_max() {
        let lines = (0..70_000)
            .map(|index| Line::from(format!("line {index}")))
            .collect::<Vec<_>>();
        let cache = prepared_search_cache(&[lines], 80);
        let found = cache.search(0, &SearchQuery::new("line 69999"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start.row, 69_999);
    }

    #[test]
    fn search_excludes_spinner_glyph_but_keeps_stable_tool_text() {
        let messages = vec![ChatMessage::ToolCall {
            id: "running".to_string(),
            name: "read".to_string(),
            target: Some("src/lib.rs".to_string()),
            status: "running".to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        }];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        prepare_exact(&mut cache, &messages, &revisions, 80, 0);

        assert!(
            cache
                .search(0, &SearchQuery::new(SPINNER_FRAMES[0]))
                .is_empty()
        );
        assert_eq!(cache.search(0, &SearchQuery::new("read")).len(), 1);
        assert_eq!(cache.search(0, &SearchQuery::new("running")).len(), 1);
    }

    #[test]
    fn reveal_offset_keeps_visible_match_and_uses_nearest_edge() {
        let cache = prepared_search_cache(
            &(0..30)
                .map(|index| vec![Line::from(format!("line {index}"))])
                .collect::<Vec<_>>(),
            80,
        );
        let found = |row, revision| {
            TranscriptSearchMatch::new(
                SelectionPos { row, col: 0 },
                SelectionPos { row, col: 4 },
                TranscriptLineIdentity {
                    message_revision: revision,
                    line_index: 0,
                },
                0..4,
            )
        };

        assert_eq!(cache.reveal_offset(0, 10, 5, &found(12, 13)), 10);
        assert_eq!(cache.reveal_offset(0, 10, 5, &found(3, 4)), 3);
        assert_eq!(cache.reveal_offset(0, 10, 5, &found(20, 21)), 16);
    }

    #[test]
    fn scroll_only_second_frame_builds_and_parses_zero_messages() {
        let messages = vec![
            ChatMessage::Assistant("# Cached\n\nMarkdown body".to_string()),
            ChatMessage::User("next prompt".to_string()),
            ChatMessage::Assistant("final answer".to_string()),
        ];
        let revisions = vec![1, 2, 3];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        let _ = cache.viewport(0, 0, 4);
        assert_eq!(builds.get(), 3);
        assert_eq!(parses.get(), 2);

        builds.set(0);
        parses.set(0);
        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        let _ = cache.viewport(0, 2, 4);

        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_eq!(cache.last_prepare_visited(), 0);
    }

    #[test]
    fn assistant_delta_rebuilds_only_the_final_message() {
        let mut messages = vec![
            ChatMessage::User("question".to_string()),
            ChatMessage::Assistant("first".to_string()),
            ChatMessage::Assistant("stream".to_string()),
        ];
        let mut revisions = vec![1, 2, 3];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            32,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        builds.set(0);
        parses.set(0);

        let ChatMessage::Assistant(text) = &mut messages[2] else {
            unreachable!();
        };
        text.push_str("ing delta");
        revisions[2] = 4;
        cache.invalidate(2);
        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            32,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );

        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);
    }

    #[test]
    fn markdown_theme_color_change_rebuilds_wrapped_lines_once() {
        let messages = vec![ChatMessage::Assistant(
            "# Heading\n\nUse `cargo test`.".to_string(),
        )];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &theme,
            theme.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        builds.set(0);
        parses.set(0);

        let mut changed = theme;
        changed.markdown_inline_code = Color::Rgb(1, 2, 3);
        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &changed,
            changed.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);

        builds.set(0);
        parses.set(0);
        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &changed,
            changed.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_eq!(cache.last_prepare_visited(), 0);
    }

    #[test]
    fn color_level_identity_rebuilds_wrapped_lines_once() {
        use crate::terminal_capabilities::TerminalColorLevel;

        let messages = vec![ChatMessage::Assistant("cached body".to_string())];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &theme,
            theme.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        builds.set(0);
        parses.set(0);

        let mut changed = theme;
        changed.color_level = TerminalColorLevel::Ansi256;
        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &changed,
            changed.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);

        builds.set(0);
        parses.set(0);
        prepare_with_theme_and_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            &changed,
            changed.syntax_theme_revision,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_eq!(cache.last_prepare_visited(), 0);
    }

    #[test]
    fn syntax_theme_revision_rebuilds_highlighted_wrapped_lines_once() {
        let messages = vec![ChatMessage::Assistant(
            "```rust\nfn highlighted() {}\n```".to_string(),
        )];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        builds.set(0);
        parses.set(0);

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            2,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);

        builds.set(0);
        parses.set(0);
        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            2,
            0,
            RenderCounters::new(&builds, &parses),
        );
        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_eq!(cache.last_prepare_visited(), 0);
    }

    #[test]
    fn spinner_frames_preserve_patch_byte_length_and_display_width() {
        let expected_byte_length = SPINNER_FRAMES[0].len();

        for frame in SPINNER_FRAMES {
            assert_eq!(frame.len(), expected_byte_length);
            assert_eq!(frame.width(), 1);
        }
    }

    #[test]
    fn spinner_patch_requires_matching_syntax_theme_revision() {
        let messages = vec![ChatMessage::ToolCall {
            id: "running".to_string(),
            name: "read".to_string(),
            target: None,
            status: "running".to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        }];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        builds.set(0);
        parses.set(0);

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            2,
            2,
            RenderCounters::new(&builds, &parses),
        );

        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 0);
    }

    #[test]
    fn builder_receives_initial_and_selectively_invalidated_message_indices() {
        let messages = vec![
            ChatMessage::System("zero".to_string()),
            ChatMessage::System("one".to_string()),
            ChatMessage::System("two".to_string()),
        ];
        let mut revisions = vec![1, 2, 3];
        let built_indices = RefCell::new(Vec::new());
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false).with_syntax_theme_revision(1),
            |index, message, _, _, _, _| {
                built_indices.borrow_mut().push(index);
                vec![Line::from(format!("{message:?}"))]
            },
        );
        assert_eq!(*built_indices.borrow(), vec![0, 1, 2]);
        assert_eq!(cache.last_prepare_visited(), messages.len());

        built_indices.borrow_mut().clear();
        revisions[1] += 1;
        cache.invalidate(1);
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false).with_syntax_theme_revision(1),
            |index, message, _, _, _, _| {
                built_indices.borrow_mut().push(index);
                vec![Line::from(format!("{message:?}"))]
            },
        );

        assert_eq!(*built_indices.borrow(), vec![1]);
        assert_eq!(cache.last_prepare_visited(), 1);
    }

    #[test]
    fn ten_thousand_messages_search_once_then_steady_scroll_and_navigation_scan_zero() {
        let messages = (0..10_000)
            .map(|index| ChatMessage::System(format!("message {index} needle")))
            .collect::<Vec<_>>();
        let revisions = (1..=10_000).collect::<Vec<u64>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );

        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("needle");
        search.refresh_with(cache.content_generation(), 0, |query| {
            cache.search(0, query)
        });
        assert_eq!(search.match_count(), 10_000);
        assert_eq!(cache.last_search_lines_visited(), 10_000);

        search.refresh_with(cache.content_generation(), 0, |_| unreachable!());
        let _ = cache.viewport(0, 5_000, 20);
        search.refresh_with(cache.content_generation(), 5_000, |_| unreachable!());
        search.next();
        search.previous();
        assert_eq!(search.scan_count_for_test(), 1);
    }

    #[test]
    fn transcript_search_ignores_unchanged_render_frames_and_rescans_only_changed_entries() {
        let messages = (0..5_000)
            .map(|index| ChatMessage::System(format!("message {index} needle")))
            .collect::<Vec<_>>();
        let mut revisions = (1..=5_000).collect::<Vec<u64>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );

        let mut search = TranscriptSearchState::default();
        search.open_new();
        search.replace_query("needle");
        search.refresh_with(cache.content_generation(), 0, |query| {
            cache.search(0, query)
        });
        assert_eq!(cache.last_search_lines_visited(), 5_000);

        for _ in 0..100 {
            search.refresh_with(cache.content_generation(), 0, |_| unreachable!());
        }
        assert_eq!(search.scan_count_for_test(), 1);

        revisions[4_999] += 1;
        cache.invalidate(4_999);
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        search.refresh_with(cache.content_generation(), 0, |query| {
            cache.search(0, query)
        });

        assert_eq!(cache.last_search_lines_visited(), 1);
        assert_eq!(search.scan_count_for_test(), 2);
        assert_eq!(search.match_count(), 5_000);
    }

    #[test]
    fn thousand_streaming_blocks_rebuild_only_the_active_tail() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        for index in 0..1_000 {
            state.update(TuiEvent::MessageDelta(format!("block {index}\n\n")));
        }

        assert_eq!(state.messages.len(), 1_000);
        assert!(
            state
                .messages
                .iter()
                .all(|message| matches!(message, ChatMessage::AssistantChunk { .. }))
        );
        let frozen_revisions = state.message_revisions.clone();
        let theme = theme();
        let built_indices = RefCell::new(Vec::new());
        let rebuilt_text_bytes = Cell::new(0);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false).with_syntax_theme_revision(1),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                if let ChatMessage::Assistant(text) | ChatMessage::AssistantChunk { text, .. } =
                    message
                {
                    rebuilt_text_bytes.set(rebuilt_text_bytes.get() + text.len());
                }
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1_000);

        built_indices.borrow_mut().clear();
        rebuilt_text_bytes.set(0);
        state.update(TuiEvent::MessageDelta("live tail\n".to_string()));
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false).with_syntax_theme_revision(1),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                if let ChatMessage::Assistant(text) | ChatMessage::AssistantChunk { text, .. } =
                    message
                {
                    rebuilt_text_bytes.set(rebuilt_text_bytes.get() + text.len());
                }
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );

        assert_eq!(*built_indices.borrow(), vec![1_000]);
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
        assert_eq!(rebuilt_text_bytes.get(), "live tail\n".len());
        assert_eq!(
            &state.message_revisions[..1_000],
            frozen_revisions.as_slice()
        );

        built_indices.borrow_mut().clear();
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false).with_syntax_theme_revision(1),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        assert!(built_indices.borrow().is_empty());
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 0);

        let _ = state.transcript_render_cache.viewport(0, 500, 20);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false).with_syntax_theme_revision(1),
            |index, message, theme, width, tick, force_expand| {
                built_indices.borrow_mut().push(index);
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        assert!(built_indices.borrow().is_empty());
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 0);

        for (width, theme, syntax_revision, force_expand) in [
            (79, theme, 1, false),
            (
                79,
                Theme::named(orca_core::config::ThemeName::Light),
                1,
                false,
            ),
            (
                79,
                Theme::named(orca_core::config::ThemeName::Light),
                2,
                false,
            ),
            (
                79,
                Theme::named(orca_core::config::ThemeName::Light),
                2,
                true,
            ),
        ] {
            built_indices.borrow_mut().clear();
            state.transcript_render_cache.prepare(
                &state.messages,
                &state.message_revisions,
                TranscriptRenderContext::new(&theme, width, 0, force_expand)
                    .with_syntax_theme_revision(syntax_revision),
                |index, message, theme, width, tick, force_expand| {
                    built_indices.borrow_mut().push(index);
                    build_lines_for_messages(
                        std::slice::from_ref(message),
                        theme,
                        width,
                        tick,
                        force_expand,
                    )
                },
            );
            assert_eq!(built_indices.borrow().len(), 1_001);
            assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1_001);
        }
    }

    #[test]
    fn assistant_chunk_selection_preserves_checkpoint_spacing_and_code() {
        let messages = vec![
            ChatMessage::AssistantChunk {
                text: "first paragraph\n\n".to_string(),
                trailing_blank: true,
            },
            ChatMessage::AssistantChunk {
                text: "```rust\nfn main() {}\n```\n".to_string(),
                trailing_blank: false,
            },
            ChatMessage::Assistant("tail".to_string()),
        ];
        let revisions = vec![1, 2, 3];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, theme, width, tick, force_expand| {
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );

        assert_eq!(
            cache.extract_text(&selection((0, 0), (99, 99))),
            "first paragraph\n\n  fn main() {}\ntail\n"
        );
    }

    #[test]
    fn tick_patches_running_or_receiving_spinners_without_rebuilding_messages() {
        let tool = |id: &str, status: &str| ChatMessage::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            target: None,
            status: status.to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        };
        let messages = vec![
            ChatMessage::Assistant("stable markdown".to_string()),
            tool("running", "running"),
            tool("receiving", "receiving"),
            tool("completed", "completed"),
        ];
        let revisions = vec![1, 2, 3, 4];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        let before = cache.entries[1].as_ref().unwrap().wrapped_lines[0]
            .text
            .clone();
        builds.set(0);
        parses.set(0);
        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            40,
            1,
            2,
            RenderCounters::new(&builds, &parses),
        );
        let after = cache.entries[1].as_ref().unwrap().wrapped_lines[0]
            .text
            .clone();

        assert_eq!(builds.get(), 0);
        assert_eq!(parses.get(), 0);
        assert_ne!(before, after);
    }

    #[test]
    fn thousands_of_messages_render_a_bounded_viewport_window() {
        let messages = (0..5_000)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone()), Line::from("")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 7_000, 20);

        assert_eq!(cache.len(), 5_000);
        assert!(viewport.rendered_message_count <= 12);
        assert!(viewport.last_message <= viewport.first_message + 12);
    }

    #[test]
    fn offsets_above_u16_max_remain_representable_and_navigable() {
        let messages = (0..40_000)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone()), Line::from("")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 70_000, 20);

        assert_eq!(viewport.total_height, 80_000);
        assert_eq!(viewport.scroll_offset, 70_000);
        assert!(viewport.first_message > u16::MAX as usize / 2);
    }

    #[test]
    fn retained_prefix_rebases_total_height_and_visible_message_indices() {
        let messages = (0..100)
            .map(|index| ChatMessage::System(format!("message {index}")))
            .collect::<Vec<_>>();
        let revisions = (1..=messages.len() as u64).collect::<Vec<_>>();
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(50, 0, 10);

        assert_eq!(viewport.total_height, 50);
        assert_eq!(viewport.first_message, 50);
        assert_eq!(viewport.last_message, 60);
        assert_eq!(viewport.lines[0].spans[0].content, "message 50");
        assert_eq!(viewport.lines[9].spans[0].content, "message 59");
    }

    #[test]
    fn tall_message_discards_complete_logical_lines_before_materializing_rows() {
        let messages = vec![
            ChatMessage::System("tall".to_string()),
            ChatMessage::System("tail".to_string()),
        ];
        let revisions = vec![1, 2];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) if text == "tall" => (0..70_000)
                    .map(|index| Line::from(format!("line {index}")))
                    .collect(),
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 69_980, 20);

        assert_eq!(viewport.scroll_offset, 69_980);
        assert!(viewport.lines.len() <= 21);
        assert_eq!(viewport.lines[0].spans[0].content, "line 69980");
    }

    #[test]
    fn tall_message_bounds_logical_lines_at_the_top_of_the_viewport() {
        let messages = vec![ChatMessage::System("tall".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, _, _, _, _, _| {
                (0..70_000)
                    .map(|index| Line::from(format!("line {index}")))
                    .collect()
            },
        );
        let viewport = cache.viewport(0, 0, 20);

        assert!(viewport.lines.len() <= 20);
        assert_eq!(viewport.lines[0].spans[0].content, "line 0");
        assert_eq!(viewport.lines[19].spans[0].content, "line 19");
    }

    #[test]
    fn trimming_stops_after_the_first_partially_visible_wrapped_line() {
        let messages = vec![
            ChatMessage::System("wrapped".to_string()),
            ChatMessage::System("later".to_string()),
        ];
        let revisions = vec![1, 2];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 5, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) if text == "wrapped" => {
                    vec![Line::from("abcdefghijklmnopqrstuvwxy")]
                }
                ChatMessage::System(_) => vec![Line::from("b"), Line::from("c")],
                _ => unreachable!(),
            },
        );
        let viewport = cache.viewport(0, 3, 4);

        assert_eq!(viewport.lines.len(), 4);
        assert_eq!(viewport.lines[0].spans[0].content, "pqrst");
        assert_eq!(viewport.lines[1].spans[0].content, "uvwxy");
        assert_eq!(viewport.lines[2].spans[0].content, "b");
        assert_eq!(viewport.lines[3].spans[0].content, "c");
    }

    #[test]
    fn one_logical_line_above_u16_rows_rebases_to_the_requested_row() {
        let body = format!("{}Z{}", "a".repeat(69_980), "b".repeat(19));
        let messages = vec![ChatMessage::System("huge".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 1, 0, false),
            |_, _, _, _, _, _| vec![Line::from(body.clone())],
        );
        let viewport = cache.viewport(0, 69_980, 20);

        assert_eq!(viewport.total_height, 70_000);
        assert_eq!(viewport.scroll_offset, 69_980);
        assert_eq!(viewport.lines[0].spans[0].content, "Z");
    }

    #[test]
    fn oversized_logical_line_bounds_materialized_content_at_the_top() {
        let body = "a".repeat(70_000);
        let messages = vec![ChatMessage::System("huge".to_string())];
        let revisions = vec![1];
        let mut cache = TranscriptRenderCache::default();
        let theme = theme();

        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 1, 0, false),
            |_, _, _, _, _, _| vec![Line::from(body.clone())],
        );
        let viewport = cache.viewport(0, 0, 20);
        let materialized_width = viewport
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.width())
            .sum::<usize>();

        assert_eq!(viewport.lines.len(), 20);
        assert_eq!(materialized_width, 20);
        assert!(
            cache.oversized_storage_segments(0, 0) <= 2,
            "the cache must not retain one Line/Span/String allocation per visual row"
        );
    }

    #[test]
    fn terminal_width_change_recomputes_layout_and_clamps_scroll() {
        let messages = vec![ChatMessage::Assistant(
            "a deliberately long markdown paragraph that wraps differently".repeat(8),
        )];
        let revisions = vec![1];
        let builds = Cell::new(0);
        let parses = Cell::new(0);
        let mut cache = TranscriptRenderCache::default();

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            80,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        let wide_height = cache.viewport(0, usize::MAX, 5).total_height;
        builds.set(0);
        parses.set(0);

        prepare_with_counters(
            &mut cache,
            &messages,
            &revisions,
            20,
            1,
            0,
            RenderCounters::new(&builds, &parses),
        );
        let narrow = cache.viewport(0, usize::MAX, 5);

        assert_eq!(builds.get(), 1);
        assert_eq!(parses.get(), 1);
        assert!(narrow.total_height > wide_height);
        assert_eq!(narrow.scroll_offset, narrow.total_height.saturating_sub(5));
    }

    #[test]
    fn transcript_reflow_is_budgeted_and_converges() {
        let messages = (0..5_000)
            .map(|index| {
                ChatMessage::System(format!(
                    "message {index}: {}",
                    "content that changes wrapped height ".repeat(3)
                ))
            })
            .collect::<Vec<_>>();
        let revisions = (1..=5_000).collect::<Vec<u64>>();
        let theme = theme();
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );

        let visible_height = 12;
        let mut requested_scroll = cache.cumulative_heights[2_500];
        let anchor_message = cache
            .viewport(0, requested_scroll, visible_height)
            .first_message;
        let mut calls = 0;
        loop {
            let visible_before = cache
                .viewport(0, requested_scroll, visible_height)
                .rendered_message_count;
            let adjusted = cache.prepare(
                &messages,
                &revisions,
                TranscriptRenderContext::new(&theme, 20, 0, false)
                    .with_reflow_window(0, requested_scroll, visible_height)
                    .with_reflow_entry_budget(32),
                |_, message, _, _, _, _| match message {
                    ChatMessage::System(text) => vec![Line::from(text.clone())],
                    _ => unreachable!(),
                },
            );
            requested_scroll = adjusted.adjusted_scroll.unwrap_or(requested_scroll);
            assert!(
                cache.last_prepare_visited() <= visible_before + 32,
                "visited {} entries with {visible_before} visible",
                cache.last_prepare_visited()
            );
            assert_eq!(
                cache
                    .viewport(0, requested_scroll, visible_height)
                    .first_message,
                anchor_message
            );
            calls += 1;
            if !cache.reflow_pending_for_test() {
                break;
            }
            assert!(calls < 200, "budgeted reflow did not converge");
        }

        let mut unlimited = TranscriptRenderCache::default();
        unlimited.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 20, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        assert_eq!(
            cache.viewport(0, 0, usize::MAX).total_height,
            unlimited.viewport(0, 0, usize::MAX).total_height
        );
    }

    #[test]
    fn transcript_reflow_has_a_default_entry_budget() {
        let messages = (0..5_000)
            .map(|index| {
                ChatMessage::System(format!(
                    "message {index}: {}",
                    "content that changes wrapped height ".repeat(3)
                ))
            })
            .collect::<Vec<_>>();
        let revisions = (1..=5_000).collect::<Vec<u64>>();
        let theme = theme();
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 80, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );

        let visible_height = 12;
        let requested_scroll = cache.cumulative_heights[2_500];
        let visible_before = cache
            .viewport(0, requested_scroll, visible_height)
            .rendered_message_count;
        cache.prepare(
            &messages,
            &revisions,
            TranscriptRenderContext::new(&theme, 20, 0, false).with_reflow_window(
                0,
                requested_scroll,
                visible_height,
            ),
            |_, message, _, _, _, _| match message {
                ChatMessage::System(text) => vec![Line::from(text.clone())],
                _ => unreachable!(),
            },
        );

        assert!(
            cache.last_prepare_visited() <= visible_before + 64,
            "default reflow visited {} entries with {visible_before} visible",
            cache.last_prepare_visited()
        );
        assert!(cache.reflow_pending_for_test());
    }
}
