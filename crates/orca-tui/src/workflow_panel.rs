//! Workflow panel navigation, background-approval dialog opening, and
//! background-task reveal routing. Extracted from `types.rs` (TUI
//! convergence slice 8).

use std::collections::VecDeque;

use orca_core::task_types::BackgroundTaskSummary;

use crate::types::{AppState, AppStatus, ApprovalDialog, PanelMode, PendingWorkflowNotification};

impl AppState {
    pub fn show_workflows(&mut self) {
        self.panel_mode = PanelMode::Workflows;
        if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
    }

    pub fn show_agents(&mut self) {
        self.panel_mode = PanelMode::Agents;
        if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
    }

    pub fn select_previous_workflow_task(&mut self) {
        self.workflow_panel.selected = self.workflow_panel.selected.saturating_sub(1);
    }

    pub fn select_next_workflow_task(&mut self) {
        let last = self.workflow_panel.tasks.len().saturating_sub(1);
        self.workflow_panel.selected = (self.workflow_panel.selected + 1).min(last);
    }
    pub fn open_selected_background_approval_dialog(&mut self) -> bool {
        let Some(task) = self.workflow_panel.tasks.get(self.workflow_panel.selected) else {
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
            .workflow_panel
            .tasks
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
            .workflow_panel
            .tasks
            .get(self.workflow_panel.selected)
            .is_some_and(is_backgrounded_running_main_session);
        let selected_task_id = self
            .workflow_panel
            .tasks
            .get(self.workflow_panel.selected)
            .map(|task| task.id.clone());
        self.workflow_panel.tasks = sort_workflow_tasks_for_panel(tasks);
        if should_reveal_background_approval {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(is_backgrounded_approval_main_session)
            {
                self.workflow_panel.selected = index;
            }
        } else if should_reveal_background_task {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(is_backgrounded_running_main_session)
            {
                self.workflow_panel.selected = index;
            }
        } else if let Some(selected_task_id) = selected_task_id
            && let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(|task| task.id == selected_task_id)
        {
            let selected_is_now_foregrounded = selected_was_backgrounded_main_session
                && is_foregrounded_running_main_session(&self.workflow_panel.tasks[index]);
            self.workflow_panel.selected = index;
            if selected_is_now_foregrounded && self.panel_mode == PanelMode::Workflows {
                self.panel_mode = PanelMode::Conversation;
            }
        } else if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
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
