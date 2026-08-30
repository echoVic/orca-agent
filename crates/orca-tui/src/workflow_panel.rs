//! Workflow panel navigation, background-approval dialog opening, and
//! background-task reveal routing. Extracted from `types.rs` (TUI
//! convergence slice 8).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crossterm::event::KeyCode;
use orca_core::task_types::{BackgroundTaskSummary, TaskType};

use crate::protocol::{PendingWorkflowNotification, TaskTranscriptRequest};
use crate::types::{AppState, AppStatus, ApprovalDialog, PanelMode};

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowPanelState {
    selected: usize,
    tasks: Vec<BackgroundTaskSummary>,
    expanded_task_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskTreeKeyResult {
    Unhandled,
    Handled,
    OpenTranscript(TaskTranscriptRequest),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkflowVisibleTask<'a> {
    pub(crate) task: &'a BackgroundTaskSummary,
    pub(crate) depth: usize,
    pub(crate) has_children: bool,
    pub(crate) expanded: bool,
}

#[derive(Clone, Debug)]
struct WorkflowTaskTree {
    rows: Vec<WorkflowTaskTreeRow>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}

#[derive(Clone, Copy, Debug)]
struct WorkflowTaskTreeRow {
    task_index: usize,
    depth: usize,
    has_children: bool,
    expanded: bool,
}

impl WorkflowPanelState {
    pub(crate) fn tasks(&self) -> &[BackgroundTaskSummary] {
        &self.tasks
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected_task(&self) -> Option<&BackgroundTaskSummary> {
        self.selected_task_index()
            .and_then(|index| self.tasks.get(index))
    }

    pub(crate) fn visible_tasks(&self) -> Vec<WorkflowVisibleTask<'_>> {
        self.task_tree()
            .rows
            .into_iter()
            .filter_map(|row| {
                self.tasks
                    .get(row.task_index)
                    .map(|task| WorkflowVisibleTask {
                        task,
                        depth: row.depth,
                        has_children: row.has_children,
                        expanded: row.expanded,
                    })
            })
            .collect()
    }

    fn replace_tasks(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        let selected_task_id = self.selected_task().map(|task| task.id.clone());
        self.tasks = sort_workflow_tasks_for_panel(tasks);
        let task_ids = self
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<HashSet<_>>();
        self.expanded_task_ids
            .retain(|task_id| task_ids.contains(task_id.as_str()));
        if let Some(selected_task_id) = selected_task_id
            && self.tasks.iter().any(|task| task.id == selected_task_id)
        {
            self.ensure_task_visible(&selected_task_id);
            self.select_task_id(&selected_task_id);
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
        self.selected = selected.min(self.task_tree().rows.len().saturating_sub(1));
    }

    fn select_task_id(&mut self, task_id: &str) {
        if let Some(index) = self
            .task_tree()
            .rows
            .iter()
            .position(|row| self.tasks[row.task_index].id == task_id)
        {
            self.selected = index;
        }
    }

    pub(crate) fn reset_for_session(&mut self) {
        self.tasks = Vec::new();
        self.selected = 0;
        self.expanded_task_ids.clear();
    }

    fn clamp_selected(&mut self) {
        self.select_index(self.selected);
    }

    pub(crate) fn handle_tree_key(&mut self, key_code: KeyCode) -> TaskTreeKeyResult {
        let tree = self.task_tree();
        let Some(row) = tree.rows.get(self.selected).copied() else {
            return TaskTreeKeyResult::Unhandled;
        };
        let task = &self.tasks[row.task_index];

        match key_code {
            KeyCode::Right if row.has_children => {
                self.expanded_task_ids.insert(task.id.clone());
                if let Some(child_index) = tree.children[row.task_index].first().copied() {
                    self.select_task_id(&self.tasks[child_index].id.clone());
                }
                TaskTreeKeyResult::Handled
            }
            KeyCode::Right => TaskTreeKeyResult::Handled,
            KeyCode::Left if row.expanded => {
                self.expanded_task_ids.remove(&task.id);
                self.clamp_selected();
                TaskTreeKeyResult::Handled
            }
            KeyCode::Left => {
                if let Some(parent_index) = tree.parents[row.task_index] {
                    self.select_task_id(&self.tasks[parent_index].id.clone());
                }
                TaskTreeKeyResult::Handled
            }
            KeyCode::Enter if task.task_type == TaskType::Subagent => {
                TaskTreeKeyResult::OpenTranscript(TaskTranscriptRequest {
                    task_id: task.id.clone(),
                    expected_revision: task.publication_revision,
                })
            }
            _ => TaskTreeKeyResult::Unhandled,
        }
    }

    fn selected_task_index(&self) -> Option<usize> {
        self.task_tree()
            .rows
            .get(self.selected)
            .map(|row| row.task_index)
    }

    fn ensure_task_visible(&mut self, task_id: &str) {
        let tree = self.task_tree();
        let Some(mut current) = self.tasks.iter().position(|task| task.id == task_id) else {
            return;
        };
        while let Some(parent) = tree.parents[current] {
            self.expanded_task_ids.insert(self.tasks[parent].id.clone());
            current = parent;
        }
    }

    fn task_tree(&self) -> WorkflowTaskTree {
        let task_index_by_id =
            self.tasks
                .iter()
                .enumerate()
                .fold(HashMap::new(), |mut indexes, (index, task)| {
                    indexes.entry(task.id.as_str()).or_insert(index);
                    indexes
                });
        let parents = (0..self.tasks.len())
            .map(|child| valid_parent_index(&self.tasks, &task_index_by_id, child))
            .collect::<Vec<_>>();
        let mut children = vec![Vec::new(); self.tasks.len()];
        for (child, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(child);
            }
        }

        let mut rows = Vec::with_capacity(self.tasks.len());
        let mut visited = HashSet::with_capacity(self.tasks.len());
        for root in (0..self.tasks.len()).filter(|index| parents[*index].is_none()) {
            self.append_visible_tree_rows(root, 0, &children, &mut visited, &mut rows);
        }

        WorkflowTaskTree {
            rows,
            parents,
            children,
        }
    }

    fn append_visible_tree_rows(
        &self,
        task_index: usize,
        depth: usize,
        children: &[Vec<usize>],
        visited: &mut HashSet<usize>,
        rows: &mut Vec<WorkflowTaskTreeRow>,
    ) {
        if !visited.insert(task_index) {
            return;
        }
        let expanded = self.expanded_task_ids.contains(&self.tasks[task_index].id);
        let has_children = !children[task_index].is_empty();
        rows.push(WorkflowTaskTreeRow {
            task_index,
            depth,
            has_children,
            expanded,
        });
        if expanded {
            for child in &children[task_index] {
                self.append_visible_tree_rows(*child, depth + 1, children, visited, rows);
            }
        }
    }

    #[cfg(test)]
    fn replace_task_tree_for_test(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        self.replace_tasks(tasks);
    }

    #[cfg(test)]
    fn visible_task_rows_for_test(&self) -> Vec<(&str, usize)> {
        self.visible_tasks()
            .into_iter()
            .map(|row| (row.task.id.as_str(), row.depth))
            .collect()
    }

    #[cfg(test)]
    fn select_task_id_for_test(&mut self, task_id: &str) {
        self.select_task_id(task_id);
    }
}

fn valid_parent_index(
    tasks: &[BackgroundTaskSummary],
    task_index_by_id: &HashMap<&str, usize>,
    child: usize,
) -> Option<usize> {
    let parent_id = tasks.get(child)?.parent_task_id.as_deref()?;
    let parent = *task_index_by_id.get(parent_id)?;
    if parent == child {
        return None;
    }

    let mut cursor = parent;
    let mut seen = HashSet::new();
    loop {
        if cursor == child || !seen.insert(cursor) {
            return None;
        }
        let Some(ancestor_id) = tasks[cursor].parent_task_id.as_deref() else {
            return Some(parent);
        };
        let Some(ancestor) = task_index_by_id.get(ancestor_id).copied() else {
            return Some(parent);
        };
        cursor = ancestor;
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

    pub(crate) fn workflow_visible_tasks(&self) -> Vec<WorkflowVisibleTask<'_>> {
        self.workflow_panel.visible_tasks()
    }

    pub(crate) fn selected_workflow_task(&self) -> Option<&BackgroundTaskSummary> {
        self.workflow_panel.selected_task()
    }

    pub(crate) fn handle_workflow_tree_key(&mut self, key_code: KeyCode) -> TaskTreeKeyResult {
        self.workflow_panel.handle_tree_key(key_code)
    }

    pub(crate) fn reset_workflow_panel(&mut self) {
        self.workflow_panel.reset_for_session();
    }

    pub fn open_selected_background_approval_dialog(&mut self) -> bool {
        let Some(task) = self.selected_workflow_task() else {
            return false;
        };
        if task.status != orca_core::task_types::TaskStatus::ApprovalRequired {
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
            if let Some(task_id) = self
                .workflow_tasks()
                .iter()
                .find(|task| is_backgrounded_approval_main_session(task))
                .map(|task| task.id.clone())
            {
                self.workflow_panel.select_task_id(&task_id);
            }
        } else if should_reveal_background_task {
            self.panel_mode = PanelMode::Workflows;
            if let Some(task_id) = self
                .workflow_tasks()
                .iter()
                .find(|task| is_backgrounded_running_main_session(task))
                .map(|task| task.id.clone())
            {
                self.workflow_panel.select_task_id(&task_id);
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

    use super::{TaskTreeKeyResult, WorkflowPanelState};
    use crate::protocol::TaskTranscriptRequest;

    fn workflow_task(id: &str, activity_at_ms: i64) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
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

    #[test]
    fn task_tree_flattens_only_expanded_children_and_repairs_selection_by_id() {
        let mut panel = WorkflowPanelState::default();
        panel.replace_task_tree_for_test(vec![
            workflow_task_with_parent("root", None),
            workflow_task_with_parent("child", Some("root")),
            workflow_task_with_parent("grandchild", Some("child")),
            workflow_task_with_parent("sibling", None),
        ]);

        assert_eq!(
            panel.visible_task_rows_for_test(),
            vec![("root", 0), ("sibling", 0)]
        );
        panel.select_task_id_for_test("root");
        assert_eq!(
            panel.handle_tree_key(crossterm::event::KeyCode::Right),
            TaskTreeKeyResult::Handled
        );
        assert_eq!(
            panel.visible_task_rows_for_test(),
            vec![("root", 0), ("child", 1), ("sibling", 0)]
        );
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("child")
        );

        panel.replace_task_tree_for_test(vec![
            workflow_task_with_parent("sibling", None),
            workflow_task_with_parent("root", None),
            workflow_task_with_parent("child", Some("root")),
            workflow_task_with_parent("grandchild", Some("child")),
        ]);
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("child")
        );
    }

    #[test]
    fn task_tree_left_collapses_before_selecting_parent() {
        let mut panel = WorkflowPanelState::default();
        panel.replace_task_tree_for_test(vec![
            workflow_task_with_parent("root", None),
            workflow_task_with_parent("child", Some("root")),
        ]);
        panel.select_task_id_for_test("root");
        assert_eq!(
            panel.handle_tree_key(crossterm::event::KeyCode::Right),
            TaskTreeKeyResult::Handled
        );
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("child")
        );
        assert_eq!(
            panel.handle_tree_key(crossterm::event::KeyCode::Left),
            TaskTreeKeyResult::Handled
        );
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("root")
        );
        assert_eq!(
            panel.handle_tree_key(crossterm::event::KeyCode::Left),
            TaskTreeKeyResult::Handled
        );
        assert_eq!(
            panel.selected_task().map(|task| task.id.as_str()),
            Some("root")
        );
        assert_eq!(panel.visible_task_rows_for_test(), vec![("root", 0)]);
    }

    #[test]
    fn task_tree_keeps_malformed_parent_references_visible_as_roots() {
        let mut panel = WorkflowPanelState::default();
        panel.replace_task_tree_for_test(vec![
            workflow_task_with_parent("missing-parent", Some("not-present")),
            workflow_task_with_parent("cycle-a", Some("cycle-b")),
            workflow_task_with_parent("cycle-b", Some("cycle-a")),
        ]);

        assert_eq!(
            panel.visible_task_rows_for_test(),
            vec![("cycle-a", 0), ("cycle-b", 0), ("missing-parent", 0)]
        );
    }

    #[test]
    fn child_enter_returns_typed_request_without_a_transcript_path() {
        let mut panel = WorkflowPanelState::default();
        let mut child = workflow_task_with_parent("child", None);
        child.task_type = TaskType::Subagent;
        child.publication_revision = Some(7);
        panel.replace_task_tree_for_test(vec![child]);

        assert_eq!(
            panel.handle_tree_key(crossterm::event::KeyCode::Enter),
            TaskTreeKeyResult::OpenTranscript(TaskTranscriptRequest {
                task_id: "child".to_string(),
                expected_revision: Some(7),
            })
        );
    }

    fn workflow_task_with_parent(id: &str, parent_task_id: Option<&str>) -> BackgroundTaskSummary {
        let mut task = workflow_task(id, 1);
        task.parent_task_id = parent_task_id.map(str::to_string);
        task
    }
}
