use std::cmp::Ordering;

use orca_core::task_types::{BackgroundTaskSummary, TaskType, WorkflowAgentTaskSummary};
use orca_core::workflow_types::WorkflowAgentStatus;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentWorkspaceIdentity {
    Task(String),
    WorkflowAgent {
        workflow_task_id: String,
        call_id: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AgentWorkspaceRow<'a> {
    Subagent {
        task: &'a BackgroundTaskSummary,
        parent: Option<&'a BackgroundTaskSummary>,
    },
    BackgroundTask {
        task: &'a BackgroundTaskSummary,
        parent: Option<&'a BackgroundTaskSummary>,
    },
    WorkflowAgent {
        workflow: &'a BackgroundTaskSummary,
        agent: &'a WorkflowAgentTaskSummary,
    },
}

impl AgentWorkspaceRow<'_> {
    pub(crate) fn identity(self) -> AgentWorkspaceIdentity {
        match self {
            Self::Subagent { task, .. } => AgentWorkspaceIdentity::Task(task.id.clone()),
            Self::BackgroundTask { task, .. } => AgentWorkspaceIdentity::Task(task.id.clone()),
            Self::WorkflowAgent { workflow, agent } => AgentWorkspaceIdentity::WorkflowAgent {
                workflow_task_id: workflow.id.clone(),
                call_id: agent.call_id.clone(),
            },
        }
    }

    pub(crate) fn is_active(self) -> bool {
        match self {
            Self::Subagent { task, .. } => task.status.is_active(),
            Self::BackgroundTask { task, .. } => task.status.is_active(),
            Self::WorkflowAgent { agent, .. } => matches!(
                agent.status,
                WorkflowAgentStatus::Pending | WorkflowAgentStatus::Running
            ),
        }
    }

    pub(crate) fn requires_attention(self) -> bool {
        matches!(self, Self::Subagent { task, .. } | Self::BackgroundTask { task, .. } if task.status.requires_attention())
    }

    fn created_at_ms(self) -> i64 {
        match self {
            Self::Subagent { task, .. } => task.created_at_ms,
            Self::BackgroundTask { task, .. } => task.created_at_ms,
            Self::WorkflowAgent { workflow, agent } => {
                agent.started_at_ms.unwrap_or(workflow.created_at_ms)
            }
        }
    }
}

pub(crate) fn agent_workspace_rows(tasks: &[BackgroundTaskSummary]) -> Vec<AgentWorkspaceRow<'_>> {
    let mut rows = Vec::new();
    for task in tasks {
        if task.task_type == TaskType::Subagent {
            let parent = task
                .parent_task_id
                .as_deref()
                .and_then(|parent_id| tasks.iter().find(|candidate| candidate.id == parent_id));
            rows.push(AgentWorkspaceRow::Subagent { task, parent });
        } else if task.task_type != TaskType::MainSession {
            let parent = task
                .parent_task_id
                .as_deref()
                .and_then(|parent_id| tasks.iter().find(|candidate| candidate.id == parent_id));
            rows.push(AgentWorkspaceRow::BackgroundTask { task, parent });
        }
        if task.task_type == TaskType::Workflow {
            rows.extend(task.workflow_agents.iter().map(|agent| {
                AgentWorkspaceRow::WorkflowAgent {
                    workflow: task,
                    agent,
                }
            }));
        }
    }
    rows.sort_by(|left, right| {
        left.created_at_ms()
            .cmp(&right.created_at_ms())
            .then_with(|| compare_identity(&left.identity(), &right.identity()))
    });
    rows
}

fn compare_identity(left: &AgentWorkspaceIdentity, right: &AgentWorkspaceIdentity) -> Ordering {
    identity_sort_key(left).cmp(&identity_sort_key(right))
}

fn identity_sort_key(identity: &AgentWorkspaceIdentity) -> (u8, &str, &str) {
    match identity {
        AgentWorkspaceIdentity::Task(task_id) => (0, task_id.as_str(), ""),
        AgentWorkspaceIdentity::WorkflowAgent {
            workflow_task_id,
            call_id,
        } => (1, workflow_task_id.as_str(), call_id.as_str()),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentWorkspaceState {
    selected: usize,
    selected_identity: Option<AgentWorkspaceIdentity>,
}

impl AgentWorkspaceState {
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_row<'a>(
        &self,
        tasks: &'a [BackgroundTaskSummary],
    ) -> Option<AgentWorkspaceRow<'a>> {
        agent_workspace_rows(tasks).get(self.selected).copied()
    }

    pub(crate) fn rows<'a>(
        &self,
        tasks: &'a [BackgroundTaskSummary],
    ) -> Vec<AgentWorkspaceRow<'a>> {
        agent_workspace_rows(tasks)
    }

    pub(crate) fn select_previous(&mut self, tasks: &[BackgroundTaskSummary]) {
        self.reconcile(tasks);
        self.selected = self.selected.saturating_sub(1);
        self.remember_selected_identity(tasks);
    }

    pub(crate) fn select_next(&mut self, tasks: &[BackgroundTaskSummary]) {
        self.reconcile(tasks);
        let row_count = agent_workspace_rows(tasks).len();
        self.selected = self
            .selected
            .saturating_add(1)
            .min(row_count.saturating_sub(1));
        self.remember_selected_identity(tasks);
    }

    pub(crate) fn select_task(&mut self, tasks: &[BackgroundTaskSummary], task_id: &str) -> bool {
        let rows = agent_workspace_rows(tasks);
        let Some(index) = rows.iter().position(|row| {
            matches!(
                row.identity(),
                AgentWorkspaceIdentity::Task(ref id) if id == task_id
            )
        }) else {
            return false;
        };
        self.selected = index;
        self.selected_identity = Some(rows[index].identity());
        true
    }

    pub(crate) fn reconcile(&mut self, tasks: &[BackgroundTaskSummary]) {
        let rows = agent_workspace_rows(tasks);
        if rows.is_empty() {
            self.selected = 0;
            self.selected_identity = None;
            return;
        }
        if let Some(identity) = self.selected_identity.as_ref()
            && let Some(index) = rows.iter().position(|row| row.identity() == *identity)
        {
            self.selected = index;
            return;
        }
        self.selected = self.selected.min(rows.len() - 1);
        self.selected_identity = Some(rows[self.selected].identity());
    }

    pub(crate) fn reset_for_session(&mut self) {
        self.selected = 0;
        self.selected_identity = None;
    }

    fn remember_selected_identity(&mut self, tasks: &[BackgroundTaskSummary]) {
        self.selected_identity = agent_workspace_rows(tasks)
            .get(self.selected)
            .map(|row| row.identity());
    }
}

#[cfg(test)]
mod tests {
    use orca_core::task_types::{
        BackgroundTaskSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    };
    use orca_core::workflow_types::WorkflowAgentStatus;

    use super::{AgentWorkspaceIdentity, AgentWorkspaceState, agent_workspace_rows};

    fn task(id: &str, task_type: TaskType, created_at_ms: i64) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: id.to_string(),
            created_at_ms,
            started_at_ms: Some(created_at_ms),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(id.to_string()),
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }
    }

    fn workflow_agent(call_id: &str, started_at_ms: i64) -> WorkflowAgentTaskSummary {
        WorkflowAgentTaskSummary {
            call_id: call_id.to_string(),
            call_path: format!("root:{call_id}"),
            team: Some("review".to_string()),
            status: WorkflowAgentStatus::Running,
            attempt: 1,
            max_attempts: 1,
            previous_errors: Vec::new(),
            error: None,
            transcript_path: None,
            started_at_ms: Some(started_at_ms),
            completed_at_ms: None,
            usage: None,
            continuation: None,
        }
    }

    #[test]
    fn rows_unify_ordinary_and_workflow_agents_in_stable_creation_order() {
        let mut workflow = task("workflow", TaskType::Workflow, 500);
        workflow.workflow_agents = vec![workflow_agent("workflow-child", 2_000)];
        let ordinary = task("ordinary-child", TaskType::Subagent, 1_000);

        let identities = agent_workspace_rows(&[workflow, ordinary])
            .into_iter()
            .map(|row| row.identity())
            .collect::<Vec<_>>();

        assert_eq!(
            identities,
            vec![
                AgentWorkspaceIdentity::Task("workflow".to_string()),
                AgentWorkspaceIdentity::Task("ordinary-child".to_string()),
                AgentWorkspaceIdentity::WorkflowAgent {
                    workflow_task_id: "workflow".to_string(),
                    call_id: "workflow-child".to_string(),
                },
            ]
        );
    }

    #[test]
    fn live_activity_updates_do_not_reorder_rows() {
        let mut first = task("first", TaskType::Subagent, 1_000);
        first.last_activity_at_ms = Some(9_000);
        let mut second = task("second", TaskType::Subagent, 2_000);
        second.last_activity_at_ms = Some(3_000);

        let identities = agent_workspace_rows(&[second, first])
            .into_iter()
            .map(|row| row.identity())
            .collect::<Vec<_>>();

        assert_eq!(
            identities,
            vec![
                AgentWorkspaceIdentity::Task("first".to_string()),
                AgentWorkspaceIdentity::Task("second".to_string()),
            ]
        );
    }

    #[test]
    fn reconcile_preserves_selected_agent_identity_across_refreshes() {
        let mut state = AgentWorkspaceState::default();
        let first = task("first", TaskType::Subagent, 1_000);
        let second = task("second", TaskType::Subagent, 2_000);
        state.reconcile(&[first.clone(), second.clone()]);
        state.select_next(&[first.clone(), second.clone()]);
        assert_eq!(
            state
                .selected_row(&[first.clone(), second.clone()])
                .map(|row| row.identity()),
            Some(AgentWorkspaceIdentity::Task("second".to_string()))
        );

        let mut refreshed_first = first;
        refreshed_first.last_activity_at_ms = Some(10_000);
        let mut refreshed_second = second;
        refreshed_second.last_activity_at_ms = Some(3_000);
        let refreshed = vec![refreshed_second, refreshed_first];
        state.reconcile(&refreshed);

        assert_eq!(state.selected(), 1);
        assert_eq!(
            state.selected_row(&refreshed).map(|row| row.identity()),
            Some(AgentWorkspaceIdentity::Task("second".to_string()))
        );
    }

    #[test]
    fn reset_clears_agent_selection() {
        let mut state = AgentWorkspaceState::default();
        let tasks = vec![task("child", TaskType::Subagent, 1_000)];
        state.reconcile(&tasks);
        assert!(state.selected_row(&tasks).is_some());

        state.reset_for_session();

        assert_eq!(state.selected(), 0);
        assert!(state.selected_row(&[]).is_none());
    }
}
