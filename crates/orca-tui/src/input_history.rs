use crate::types::AppState;

fn input_history_path() -> Option<std::path::PathBuf> {
    // Unit tests must never read or pollute the real user history.
    if cfg!(test) {
        return None;
    }
    dirs::home_dir().map(|h| h.join(".orca").join("history.jsonl"))
}

fn current_project() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

pub(crate) fn load_input_history() -> Vec<String> {
    let Some(path) = input_history_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let project = current_project();
    const MAX: usize = 500;
    // Read all valid entries, newest-first (reverse lines), project entries first
    let entries: Vec<(String, bool)> = content
        .lines()
        .rev()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let display = v["display"].as_str()?.to_string();
            let is_current = v["project"].as_str().unwrap_or("") == project;
            Some((display, is_current))
        })
        .take(MAX * 2)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    // Current project first
    for (display, is_current) in &entries {
        if *is_current && seen.insert(display.clone()) {
            result.push(display.clone());
            if result.len() >= MAX {
                break;
            }
        }
    }
    // Then other projects
    for (display, is_current) in &entries {
        if !is_current && seen.insert(display.clone()) {
            result.push(display.clone());
            if result.len() >= MAX {
                break;
            }
        }
    }
    result.reverse();
    result
}

fn append_input_history(prompt: &str) {
    let Some(path) = input_history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = serde_json::json!({
        "display": prompt,
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        "project": current_project(),
    });
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", entry);
    }
}

impl AppState {
    pub fn record_prompt(&mut self, prompt: String) {
        if self
            .input_history
            .last()
            .map(|last| last != &prompt)
            .unwrap_or(true)
        {
            self.input_history.push(prompt.clone());
            append_input_history(&prompt);
        }
        self.history_cursor = None;
        self.draft_before_history = None;
    }

    pub fn history_previous(&mut self, current_draft: String) -> Option<String> {
        if self.input_history.is_empty() {
            return None;
        }

        let next = match self.history_cursor {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                self.draft_before_history = Some(current_draft);
                self.input_history.len() - 1
            }
        };
        self.history_cursor = Some(next);
        self.input_history.get(next).cloned()
    }

    pub fn history_next(&mut self) -> Option<String> {
        let cursor = self.history_cursor?;
        let next = cursor + 1;

        if next >= self.input_history.len() {
            self.history_cursor = None;
            return Some(self.draft_before_history.take().unwrap_or_default());
        }

        self.history_cursor = Some(next);
        self.input_history.get(next).cloned()
    }

    pub fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.draft_before_history = None;
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::UserAction;
    use crate::types::AppState;

    fn state() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded::<UserAction>();
        AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    #[test]
    fn history_navigation_clamps_and_restores_the_unsent_draft() {
        let mut state = state();
        state.input_history = vec!["first".to_string(), "second".to_string()];

        assert_eq!(
            state.history_previous("draft".to_string()).as_deref(),
            Some("second")
        );
        assert_eq!(
            state.history_previous("ignored".to_string()).as_deref(),
            Some("first")
        );
        assert_eq!(state.history_previous("ignored".to_string()), None);
        assert_eq!(state.history_next().as_deref(), Some("second"));
        assert_eq!(state.history_next().as_deref(), Some("draft"));
        assert_eq!(state.history_next(), None);
        assert!(state.history_cursor.is_none());
        assert!(state.draft_before_history.is_none());

        assert_eq!(
            state
                .history_previous("another draft".to_string())
                .as_deref(),
            Some("second")
        );
        state.reset_history_navigation();
        assert!(state.history_cursor.is_none());
        assert!(state.draft_before_history.is_none());
    }
}
