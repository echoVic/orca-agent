//! Workflow panel navigation, background-approval dialog opening, and
//! background-task reveal routing. Extracted from `types.rs` (TUI
//! convergence slice 8).

use std::collections::VecDeque;

use orca_core::task_types::BackgroundTaskSummary;

use crate::protocol::PendingWorkflowNotification;
use crate::types::{AppState, AppStatus, ApprovalDialog, PanelMode};

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowPanelState {
    selected: usize,
    tasks: Vec<BackgroundTaskSummary>,
}

impl WorkflowPanelState {
    pub(crate) fn tasks(&self) -> &[BackgroundTaskSummary] {
        &self.tasks
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_task(&self) -> Option<&BackgroundTaskSummary> {
        self.tasks.get(self.selected)
    }

    fn replace_tasks(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        let selected_task_id = self.selected_task().map(|task| task.id.clone());
        self.tasks = sort_workflow_tasks_for_panel(tasks);
        if let Some(selected_task_id) = selected_task_id
            && let Some(index) = self
                .tasks
                .iter()
                .position(|task| task.id == selected_task_id)
        {
            self.selected = index;
        } else {
            self.clamp_selected();
        }
    }

    fn select_previous(&mut self) {
        self.select_index(self.selected.saturating_sub(1));
    }

    fn select_next(&mut self) {
        self.select_index(self.selected.saturating_add(1));
    }

    fn select_index(&mut self, selected: usize) {
        self.selected = selected.min(self.tasks.len().saturating_sub(1));
    }

    pub(crate) fn reset_for_session(&mut self) {
        self.tasks = Vec::new();
        self.selected = 0;
    }

    fn clamp_selected(&mut self) {
        self.select_index(self.selected);
    }
}

impl AppState {
    pub fn show_workflows(&mut self) {
        self.panel_mode = PanelMode::Workflows;
        self.workflow_panel.clamp_selected();
    }

    pub fn show_agents(&mut self) {
        self.panel_mode = PanelMode::Agents;
        self.workflow_panel.clamp_selected();
    }

    pub fn select_previous_workflow_task(&mut self) {
        self.workflow_panel.select_previous();
    }

    pub fn select_next_workflow_task(&mut self) {
        self.workflow_panel.select_next();
    }

    pub(crate) fn workflow_tasks(&self) -> &[BackgroundTaskSummary] {
        self.workflow_panel.tasks()
    }

    pub(crate) fn workflow_selected_index(&self) -> usize {
        self.workflow_panel.selected()
    }

    pub(crate) fn selected_workflow_task(&self) -> Option<&BackgroundTaskSummary> {
        self.workflow_panel.selected_task()
    }

    pub(crate) fn reset_workflow_panel(&mut self) {
        self.workflow_panel.reset_for_session();
    }

    pub fn open_selected_background_approval_dialog(&mut self) -> bool {
        let Some(task) = self.selected_workflow_task() else {
            return false;
        };
        if task.task_type != orca_core::task_types::TaskType::MainSession
            || task.status != orca_core::task_types::TaskStatus::ApprovalRequired
        {
            return false;
        }
        let Some(pending_tool_call) = task.pending_tool_call.as_ref() else {
            return false;
        };

        let id = pending_tool_call.id.clone();
        let tool = pending_tool_call.name.clone();
        let target = pending_tool_call.target.clone();
        let background_task_id = task.id.clone();
        let preview = pending_tool_call.arguments.clone();
        let options = ApprovalDialog::options_for(&tool, target.as_deref());
        self.set_status(AppStatus::WaitingApproval);
        self.approval_dialog = Some(ApprovalDialog {
            id,
            interaction: None,
            tool,
            target,
            permission_kind: None,
            background_task_id: Some(background_task_id),
            selected: 0,
            options,
            diff: Some(preview),
        });
        true
    }

    pub(crate) fn push_pending_workflow_notification(
        &mut self,
        notification: PendingWorkflowNotification,
    ) -> bool {
        push_pending_workflow_notification_unique(
            &mut self.pending_workflow_notifications,
            notification,
        )
    }

    pub fn show_conversation(&mut self) {
        self.panel_mode = PanelMode::Conversation;
    }

    pub(crate) fn apply_workflow_tasks_update(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        let was_suppressing_background_output = self.suppress_background_main_session_output;
        let has_backgrounded_running_main_session =
            tasks.iter().any(is_backgrounded_running_main_session);
        let had_backgrounded_approval_main_session = self
            .workflow_tasks()
            .iter()
            .any(is_backgrounded_approval_main_session);
        let has_backgrounded_approval_main_session =
            tasks.iter().any(is_backgrounded_approval_main_session);
        self.suppress_background_main_session_output = has_backgrounded_running_main_session;
        if has_backgrounded_running_main_session {
            self.set_status(AppStatus::Idle);
        }
        let should_reveal_background_task =
            has_backgrounded_running_main_session && !was_suppressing_background_output;
        let should_reveal_background_approval =
            has_backgrounded_approval_main_session && !had_backgrounded_approval_main_session;
        let selected_was_backgrounded_main_session = self
            .selected_workflow_task()
            .is_some_and(is_backgrounded_running_main_session);
        let selected_task_id = self.selected_workflow_task().map(|task| task.id.clone());
        self.workflow_panel.replace_tasks(tasks);
        if should_reveal_background_approval {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_tasks()
                .iter()
                .position(is_backgrounded_approval_main_session)
            {
                self.workflow_panel.select_index(index);
            }
        } else if should_reveal_background_task {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_tasks()
                .iter()
                .position(is_backgrounded_running_main_session)
            {
                self.workflow_panel.select_index(index);
            }
        } else if let Some(selected_task_id) = selected_task_id
            && let Some(selected_task) = self.selected_workflow_task()
            && selected_task.id == selected_task_id
        {
            let selected_is_now_foregrounded = selected_was_backgrounded_main_session
                && is_foregrounded_running_main_session(selected_task);
            if selected_is_now_foregrounded && self.panel_mode == PanelMode::Workflows {
                self.panel_mode = PanelMode::Conversation;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_workflow_tasks_for_test(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        self.workflow_panel.replace_tasks(tasks);
    }

    #[cfg(test)]
    pub(crate) fn apply_workflow_tasks_for_test(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        self.apply_workflow_tasks_update(tasks);
    }

    #[cfg(test)]
    pub(crate) fn select_workflow_index_for_test(&mut self, selected: usize) {
        self.workflow_panel.select_index(selected);
    }
}

pub(crate) fn push_pending_workflow_notification_unique(
    queue: &mut VecDeque<PendingWorkflowNotification>,
    notification: PendingWorkflowNotification,
) -> bool {
    if queue.iter().any(|pending| pending.id == notification.id) {
        return false;
    }
    queue.push_back(notification);
    true
}

pub(crate) fn sort_workflow_tasks_for_panel(
    mut tasks: Vec<BackgroundTaskSummary>,
) -> Vec<BackgroundTaskSummary> {
    tasks.sort_by(|left, right| {
        workflow_task_panel_group(left)
            .cmp(&workflow_task_panel_group(right))
            .then_with(|| workflow_task_activity_ms(right).cmp(&workflow_task_activity_ms(left)))
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    tasks
}

fn is_backgrounded_running_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::Running
        && task.is_backgrounded
}

fn is_backgrounded_approval_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
        && task.is_backgrounded
        && task.pending_tool_call.is_some()
}

fn is_foregrounded_running_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::Running
        && !task.is_backgrounded
}

fn workflow_task_panel_group(task: &BackgroundTaskSummary) -> u8 {
    match task.status {
        orca_core::task_types::TaskStatus::ApprovalRequired => 0,
        orca_core::task_types::TaskStatus::Queued
        | orca_core::task_types::TaskStatus::Running
        | orca_core::task_types::TaskStatus::Paused
        | orca_core::task_types::TaskStatus::Stopping => 1,
        orca_core::task_types::TaskStatus::Stopped
        | orca_core::task_types::TaskStatus::Completed
        | orca_core::task_types::TaskStatus::Failed
        | orca_core::task_types::TaskStatus::Cancelled => 2,
    }
}

fn workflow_task_activity_ms(task: &BackgroundTaskSummary) -> i64 {
    task.last_activity_at_ms
        .or(task.completed_at_ms)
        .or(task.started_at_ms)
        .unwrap_or(task.created_at_ms)
}

#[cfg(test)]
mod tests {
    use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};

    use super::WorkflowPanelState;

    fn workflow_task(id: &str, activity_at_ms: i64) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: id.to_string(),
            created_at_ms: activity_at_ms,
            started_at_ms: Some(activity_at_ms),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(id.to_string()),
            workflow_run_id: Some(format!("run-{id}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(activity_at_ms),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }
    }

    #[test]
    fn workflow_panel_owner_sorts_preserves_selection_and_clears() {
        let mut panel = WorkflowPanelState::default();
        panel.replace_tasks(vec![workflow_task("later", 20), workflow_task("first", 10)]);
        panel.select_index(0);
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("later")
        );

        panel.replace_tasks(vec![workflow_task("later", 30), workflow_task("new", 40)]);
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("later")
        );

        panel.select_index(usize::MAX);
        assert_eq!(panel.selected(), 1);
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("later")
        );

        panel.reset_for_session();
        panel.select_index(usize::MAX);
        assert!(panel.tasks().is_empty());
        assert_eq!(panel.selected(), 0);
    }
}
