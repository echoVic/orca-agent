//! Saved-session picker state: filtered indices, selection navigation, and
//! query editing with the first-match reset invariant. Extracted from
//! `types.rs` (TUI convergence slice 10).

use orca_runtime::history::SessionSummary;

use crate::types::AppState;

/// One row of the rendered picker: either a project group header, or a
/// session belonging to the group immediately above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionPickerRow {
    Group { label: String, current: bool },
    Session(usize),
}

/// Sessions written by the test suite are tagged with the `mock` provider so
/// they can stay out of the picker until explicitly requested.
fn is_test_session(session: &SessionSummary) -> bool {
    session.provider == "mock"
}

/// Last path segment of `cwd`, used as the group label. Falls back to the
/// full path when it has no file-name component (e.g. `/`).
fn project_label(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

impl AppState {
    /// Indices into `session_picker_sessions` whose title matches the current
    /// query (case-insensitive substring) and, unless
    /// `session_picker_show_tests` is set, are not test-suite sessions.
    /// Empty query matches every title.
    pub fn filtered_session_indices(&self) -> Vec<usize> {
        let needle = self.session_picker_query.to_lowercase();
        self.session_picker_sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| self.session_picker_show_tests || !is_test_session(session))
            .filter(|(_, session)| {
                needle.is_empty() || session.title.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// How many sessions the mock-session filter is currently hiding. Always
    /// zero once `session_picker_show_tests` is set.
    pub(crate) fn hidden_test_session_count(&self) -> usize {
        if self.session_picker_show_tests {
            return 0;
        }
        self.session_picker_sessions
            .iter()
            .filter(|session| is_test_session(session))
            .count()
    }

    /// Filtered sessions grouped by project directory. The current project
    /// comes first; other groups follow by their most recent session;
    /// sessions inside a group are newest first.
    pub(crate) fn session_picker_rows(&self) -> Vec<SessionPickerRow> {
        let mut groups: Vec<(String, bool, Vec<usize>)> = Vec::new();
        for index in self.filtered_session_indices() {
            let session = &self.session_picker_sessions[index];
            let current = session.cwd == self.workspace_path;
            match groups.iter_mut().find(|(cwd, _, _)| *cwd == session.cwd) {
                Some((_, _, members)) => members.push(index),
                None => groups.push((session.cwd.clone(), current, vec![index])),
            }
        }
        for (_, _, members) in groups.iter_mut() {
            members.sort_by(|a, b| {
                let a_time = self.session_picker_sessions[*a].updated_at;
                let b_time = self.session_picker_sessions[*b].updated_at;
                b_time.cmp(&a_time)
            });
        }
        groups.sort_by(|(_, a_current, a_members), (_, b_current, b_members)| {
            let a_newest = a_members
                .iter()
                .map(|index| self.session_picker_sessions[*index].updated_at)
                .max();
            let b_newest = b_members
                .iter()
                .map(|index| self.session_picker_sessions[*index].updated_at)
                .max();
            // Current project first, then most-recently-active group first.
            b_current.cmp(a_current).then(b_newest.cmp(&a_newest))
        });
        let mut rows = Vec::new();
        for (cwd, current, members) in groups {
            rows.push(SessionPickerRow::Group {
                label: project_label(&cwd),
                current,
            });
            rows.extend(members.into_iter().map(SessionPickerRow::Session));
        }
        rows
    }

    /// Session indices in the order the picker draws them: by project group,
    /// newest first within each. Navigation walks this order, so the
    /// highlight moves to the row below rather than to the next session by
    /// time, which may sit in another group.
    pub(crate) fn visible_session_order(&self) -> Vec<usize> {
        self.session_picker_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionPickerRow::Session(index) => Some(index),
                SessionPickerRow::Group { .. } => None,
            })
            .collect()
    }

    pub fn select_previous_session(&mut self) {
        let filtered = self.visible_session_order();
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
        let filtered = self.visible_session_order();
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
        let filtered = self.visible_session_order();
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
        let filtered = self.visible_session_order();
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
        if let Some(&first) = self.visible_session_order().first() {
            self.session_picker_selected = first;
        }
    }

    pub fn select_last_session(&mut self) {
        if let Some(&last) = self.visible_session_order().last() {
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

    /// Reset the selection to the first row the current filter/query shows.
    /// When nothing matches, moves the selection out of range (one past the
    /// last loaded session) instead of leaving it on a stale index that a
    /// filter change may have just hidden. Every consumer of
    /// `session_picker_selected` already treats an out-of-range index as "no
    /// selection" (`Vec::get`), so this is a clean, panic-free sentinel.
    pub(crate) fn reset_session_selection_to_first_match(&mut self) {
        self.session_picker_selected = self
            .visible_session_order()
            .first()
            .copied()
            .unwrap_or(self.session_picker_sessions.len());
    }

    pub fn selected_session_id(&self) -> Option<String> {
        self.session_picker_sessions
            .get(self.session_picker_selected)
            .map(|session| session.session_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel as mpsc;
    use orca_runtime::history::SessionSummary;

    fn summary_defaults() -> SessionSummary {
        SessionSummary {
            session_id: String::new(),
            title: String::new(),
            cwd: String::new(),
            provider: String::new(),
            model: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            path: std::path::PathBuf::new(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
            health: orca_runtime::history::StoredSessionHealth::Healthy,
            health_issue: None,
            source_fingerprint: None,
            storage_identity: String::new(),
        }
    }

    fn summary(title: &str, cwd: &str, provider: &str, minutes_ago: i64) -> SessionSummary {
        let now = chrono::Utc::now();
        SessionSummary {
            session_id: format!("id-{title}"),
            title: title.into(),
            cwd: cwd.into(),
            provider: provider.into(),
            model: None,
            created_at: now - chrono::Duration::minutes(minutes_ago),
            updated_at: now - chrono::Duration::minutes(minutes_ago),
            ..summary_defaults()
        }
    }

    fn test_state_in(cwd: &str) -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(tx, "0.0.0".into(), "deepseek".into(), cwd.into())
    }

    #[test]
    fn mock_sessions_are_hidden_until_requested() {
        let mut state = test_state_in("/work/orca");
        state.session_picker_sessions = vec![
            summary("real", "/work/orca", "deepseek", 5),
            summary("schema_ok", "/work/orca", "mock", 1),
        ];
        assert_eq!(state.filtered_session_indices(), vec![0]);
        assert_eq!(state.hidden_test_session_count(), 1);
        state.session_picker_show_tests = true;
        assert_eq!(state.filtered_session_indices(), vec![0, 1]);
    }

    #[test]
    fn reset_selection_to_first_match_falls_back_out_of_range_when_nothing_matches() {
        let mut state = test_state_in("/work/orca");
        state.session_picker_sessions = vec![summary("only", "/work/orca", "mock", 1)];
        state.session_picker_selected = 0;

        state.reset_session_selection_to_first_match();

        // The only loaded session is a hidden mock session, so nothing
        // matches; the selection must move out of range rather than stay on
        // a session the user can no longer see.
        assert_eq!(
            state.session_picker_selected,
            state.session_picker_sessions.len()
        );
        assert_eq!(state.selected_session_id(), None);
    }

    #[test]
    fn picker_rows_group_by_project_with_the_current_project_first() {
        let mut state = test_state_in("/work/orca");
        state.session_picker_sessions = vec![
            summary("other newest", "/work/other", "deepseek", 1),
            summary("orca older", "/work/orca", "deepseek", 60),
            summary("orca newer", "/work/orca", "deepseek", 30),
        ];
        let rows = state.session_picker_rows();
        assert!(
            matches!(&rows[0], SessionPickerRow::Group { label, current: true } if label == "orca")
        );
        assert!(matches!(rows[1], SessionPickerRow::Session(2)));
        assert!(matches!(rows[2], SessionPickerRow::Session(1)));
        assert!(
            matches!(&rows[3], SessionPickerRow::Group { label, current: false } if label == "other")
        );
        assert!(matches!(rows[4], SessionPickerRow::Session(0)));
    }

    #[test]
    fn the_current_project_is_recognised_under_the_home_directory() {
        // The status bar shows a workspace under $HOME as "~/…"; sessions
        // record the absolute path.
        let mut state = test_state_in("~/work/orca");
        state.workspace_path = "/Users/me/work/orca".to_string();
        state.session_picker_sessions = vec![
            summary("other newest", "/Users/me/work/other", "deepseek", 1),
            summary("orca", "/Users/me/work/orca", "deepseek", 30),
        ];
        let rows = state.session_picker_rows();
        assert!(
            matches!(&rows[0], SessionPickerRow::Group { label, current: true } if label == "orca"),
            "{rows:?}"
        );
        assert!(matches!(rows[1], SessionPickerRow::Session(1)));
    }

    #[test]
    fn the_selection_moves_through_the_rows_as_they_are_drawn() {
        let mut state = test_state_in("/work/orca");
        state.session_picker_sessions = vec![
            summary("other A", "/work/other", "deepseek", 1),
            summary("orca B", "/work/orca", "deepseek", 2),
            summary("other C", "/work/other", "deepseek", 3),
            summary("orca D", "/work/orca", "deepseek", 4),
        ];
        // Drawn: orca [B, D], then other [A, C].
        let title = |state: &AppState| {
            state.session_picker_sessions[state.session_picker_selected]
                .title
                .clone()
        };

        state.reset_session_selection_to_first_match();
        assert_eq!(title(&state), "orca B");
        let mut walked = vec![title(&state)];
        for _ in 0..4 {
            state.select_next_session();
            walked.push(title(&state));
        }
        assert_eq!(
            walked,
            ["orca B", "orca D", "other A", "other C", "other C"]
        );

        state.select_previous_session();
        assert_eq!(title(&state), "other A");
        state.select_first_session();
        assert_eq!(title(&state), "orca B");
        state.select_last_session();
        assert_eq!(title(&state), "other C");
        state.select_session_page_up();
        assert_eq!(title(&state), "orca B");
        state.select_session_page_down();
        assert_eq!(title(&state), "other C");
    }
}
