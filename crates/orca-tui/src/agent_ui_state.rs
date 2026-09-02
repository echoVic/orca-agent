use orca_core::agent_event::{AgentRegistrySnapshot, AgentSummary};

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentUiState {
    root_thread_id: Option<String>,
    focused_thread_id: Option<String>,
    selected_dock_index: usize,
    agents: Vec<AgentSummary>,
}

impl AgentUiState {
    pub(crate) fn apply(
        &mut self,
        root_thread_id: String,
        snapshot: AgentRegistrySnapshot,
    ) -> bool {
        if self
            .root_thread_id
            .as_ref()
            .is_some_and(|current| current != &root_thread_id)
        {
            *self = Self::default();
        }
        self.root_thread_id = Some(root_thread_id);
        let selected_thread = self.selected_thread_id().map(str::to_string);
        self.agents = snapshot.agents;
        self.selected_dock_index = selected_thread
            .as_deref()
            .and_then(|thread_id| {
                self.agents
                    .iter()
                    .position(|agent| agent.thread_id == thread_id)
                    .map(|index| index + 1)
            })
            .unwrap_or_else(|| self.selected_dock_index.min(self.agents.len()));
        if self
            .focused_thread_id
            .as_ref()
            .is_some_and(|focused| !self.agents.iter().any(|agent| &agent.thread_id == focused))
        {
            self.focused_thread_id = None;
        }
        true
    }

    pub(crate) fn focused_thread_id(&self) -> Option<&str> {
        self.focused_thread_id.as_deref()
    }

    pub(crate) fn selected_dock_index(&self) -> usize {
        self.selected_dock_index
    }

    pub(crate) fn agents(&self) -> &[AgentSummary] {
        &self.agents
    }

    pub(crate) fn selected_thread_id(&self) -> Option<&str> {
        self.selected_dock_index
            .checked_sub(1)
            .and_then(|index| self.agents.get(index))
            .map(|agent| agent.thread_id.as_str())
    }

    pub(crate) fn select_previous(&mut self) {
        self.selected_dock_index = self.selected_dock_index.saturating_sub(1);
    }

    pub(crate) fn select_next(&mut self) {
        self.selected_dock_index = self
            .selected_dock_index
            .saturating_add(1)
            .min(self.agents.len());
    }

    pub(crate) fn focus_selected(&mut self) -> Option<String> {
        let focused = self.selected_thread_id()?.to_string();
        self.focused_thread_id = Some(focused.clone());
        Some(focused)
    }

    pub(crate) fn focus_thread(&mut self, thread_id: Option<String>) {
        self.focused_thread_id = thread_id;
        if let Some(focused) = self.focused_thread_id.as_deref()
            && let Some(index) = self
                .agents
                .iter()
                .position(|agent| agent.thread_id == focused)
        {
            self.selected_dock_index = index + 1;
        } else if self.focused_thread_id.is_none() {
            self.selected_dock_index = 0;
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use orca_core::agent_event::{AgentRegistrySnapshot, AgentStatus, AgentSummary};

    use super::AgentUiState;

    fn agent(id: &str, created_at_ms: i64) -> AgentSummary {
        AgentSummary {
            root_thread_id: "root".to_string(),
            batch_id: "batch".to_string(),
            batch_size: 2,
            agent_id: id.to_string(),
            thread_id: format!("thread-{id}"),
            parent_thread_id: "root".to_string(),
            description: id.to_string(),
            status: AgentStatus::Running,
            activity: None,
            turn: None,
            usage: Default::default(),
            result: None,
            error: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }

    #[test]
    fn dock_includes_main_and_preserves_selected_thread() {
        let mut state = AgentUiState::default();
        state.apply(
            "root".to_string(),
            AgentRegistrySnapshot {
                revision: 1,
                agents: vec![agent("a", 1), agent("b", 2)],
            },
        );
        state.select_next();
        state.select_next();
        assert_eq!(state.selected_thread_id(), Some("thread-b"));

        state.apply(
            "root".to_string(),
            AgentRegistrySnapshot {
                revision: 2,
                agents: vec![agent("b", 2), agent("a", 1)],
            },
        );
        assert_eq!(state.selected_thread_id(), Some("thread-b"));
    }
}
