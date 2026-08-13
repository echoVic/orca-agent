//! Saved-session picker state: filtered indices, selection navigation, and
//! query editing with the first-match reset invariant. Extracted from
//! `types.rs` (TUI convergence slice 10).

use crate::types::AppState;

impl AppState {
    /// Indices into `session_picker_sessions` whose title matches the current
    /// query (case-insensitive substring). Empty query matches everything.
    pub fn filtered_session_indices(&self) -> Vec<usize> {
        if self.session_picker_query.is_empty() {
            return (0..self.session_picker_sessions.len()).collect();
        }
        let needle = self.session_picker_query.to_lowercase();
        self.session_picker_sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.title.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn select_previous_session(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = pos.saturating_sub(1);
        self.session_picker_selected = filtered[new_pos];
    }

    pub fn select_next_session(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = (pos + 1).min(filtered.len() - 1);
        self.session_picker_selected = filtered[new_pos];
    }

    pub fn select_session_page_up(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = pos.saturating_sub(10);
        self.session_picker_selected = filtered[new_pos];
    }

    pub fn select_session_page_down(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = (pos + 10).min(filtered.len() - 1);
        self.session_picker_selected = filtered[new_pos];
    }

    pub fn select_first_session(&mut self) {
        if let Some(&first) = self.filtered_session_indices().first() {
            self.session_picker_selected = first;
        }
    }

    pub fn select_last_session(&mut self) {
        if let Some(&last) = self.filtered_session_indices().last() {
            self.session_picker_selected = last;
        }
    }

    /// Append a character to the search query and reset selection to the first
    /// match so the highlighted row is always within the filtered set.
    pub fn session_query_push(&mut self, ch: char) {
        self.session_picker_query.push(ch);
        self.reset_session_selection_to_first_match();
    }

    pub fn session_query_pop(&mut self) {
        self.session_picker_query.pop();
        self.reset_session_selection_to_first_match();
    }

    pub(crate) fn reset_session_selection_to_first_match(&mut self) {
        if let Some(&first) = self.filtered_session_indices().first() {
            self.session_picker_selected = first;
        }
    }

    pub fn selected_session_id(&self) -> Option<String> {
        self.session_picker_sessions
            .get(self.session_picker_selected)
            .map(|session| session.session_id.clone())
    }
}
