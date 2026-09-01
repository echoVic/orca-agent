use crossbeam_channel as mpsc;
use crossterm::event::KeyCode;
use orca_core::task_types::{TaskStatus, TaskType};

use crate::agent_workspace::AgentWorkspaceRow;
use crate::protocol::{TaskTranscriptRequest, UserAction};
use crate::types::{AppState, PanelMode};

pub(crate) fn handle_agent_workspace_key(
    key_code: KeyCode,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    if state.panel_mode != PanelMode::Agents {
        return false;
    }

    if state.task_transcript().is_some() {
        match key_code {
            KeyCode::Up => state.scroll_task_transcript_up(),
            KeyCode::Down => state.scroll_task_transcript_down(),
            KeyCode::Home => {
                while state.task_transcript_scroll() > 0 {
                    state.scroll_task_transcript_up();
                }
            }
            KeyCode::Enter | KeyCode::Char('s') => {}
            _ => return false,
        }
        return true;
    }

    match key_code {
        KeyCode::Up => {
            state.select_previous_agent();
            true
        }
        KeyCode::Down => {
            state.select_next_agent();
            true
        }
        KeyCode::Enter => {
            let request = match state.selected_agent_row() {
                Some(AgentWorkspaceRow::Subagent { task, .. }) => {
                    task.publication_revision
                        .map(|expected_revision| TaskTranscriptRequest {
                            task_id: task.id.clone(),
                            expected_revision,
                        })
                }
                Some(AgentWorkspaceRow::BackgroundTask { .. })
                | Some(AgentWorkspaceRow::WorkflowAgent { .. })
                | None => None,
            };
            if let Some(request) = request {
                state.begin_task_transcript_request(request.clone());
                let _ = action_tx.send(UserAction::ReadTaskTranscript(request));
            }
            true
        }
        KeyCode::Char('s') => {
            let task_id = match state.selected_agent_row() {
                Some(AgentWorkspaceRow::Subagent { task, .. })
                    if !matches!(
                        task.status,
                        TaskStatus::Completed
                            | TaskStatus::Failed
                            | TaskStatus::Cancelled
                            | TaskStatus::Stopped
                    ) =>
                {
                    Some(task.id.clone())
                }
                Some(AgentWorkspaceRow::BackgroundTask { task, .. })
                    if task.task_type != TaskType::Workflow
                        && !matches!(
                            task.status,
                            TaskStatus::Completed
                                | TaskStatus::Failed
                                | TaskStatus::Cancelled
                                | TaskStatus::Stopped
                        ) =>
                {
                    Some(task.id.clone())
                }
                _ => None,
            };
            if let Some(task_id) = task_id {
                let _ = action_tx.send(UserAction::StopTask { task_id });
            }
            true
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            let action = state.selected_agent_row().and_then(|row| match row {
                AgentWorkspaceRow::Subagent { task, .. }
                    if task.continuation.as_ref().is_some_and(|continuation| {
                        continuation.resumable && !continuation.indeterminate
                    }) =>
                {
                    Some(if key_code == KeyCode::Char('R') {
                        UserAction::RetryTask {
                            task_id: task.id.clone(),
                        }
                    } else {
                        UserAction::ResumeTask {
                            task_id: task.id.clone(),
                        }
                    })
                }
                _ => None,
            });
            if let Some(action) = action {
                let _ = action_tx.send(action);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use crossterm::event::KeyCode;
    use orca_core::task_types::{
        BackgroundTaskSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    };
    use orca_core::workflow_types::WorkflowAgentStatus;

    use super::handle_agent_workspace_key;
    use crate::protocol::{TaskTranscriptRequest, UserAction};
    use crate::types::{AppState, PanelMode};

    fn task(id: &str, created_at_ms: i64) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type: TaskType::Subagent,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: id.to_string(),
            created_at_ms,
            started_at_ms: Some(created_at_ms),
            completed_at_ms: None,
            command: None,
            agent_type: Some("general".to_string()),
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
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: Some(7),
        }
    }

    fn state(tasks: Vec<BackgroundTaskSummary>) -> (AppState, mpsc::Receiver<UserAction>) {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.replace_workflow_tasks_for_test(tasks);
        state.show_agents();
        (state, rx)
    }

    #[test]
    fn arrows_select_agents_and_enter_requests_the_selected_typed_transcript() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.replace_workflow_tasks_for_test(vec![task("first", 1_000), task("second", 2_000)]);
        state.show_agents();

        assert!(handle_agent_workspace_key(KeyCode::Down, &mut state, &tx));
        assert_eq!(state.agent_selected_index(), 1);
        assert!(handle_agent_workspace_key(KeyCode::Up, &mut state, &tx));
        assert_eq!(state.agent_selected_index(), 0);
        assert!(handle_agent_workspace_key(KeyCode::Down, &mut state, &tx));
        assert!(handle_agent_workspace_key(KeyCode::Enter, &mut state, &tx));

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ReadTaskTranscript(TaskTranscriptRequest {
                task_id,
                expected_revision: 7,
            })) if task_id == "second"
        ));
    }

    #[test]
    fn stop_targets_only_the_selected_live_ordinary_subagent() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.replace_workflow_tasks_for_test(vec![task("child", 1_000)]);
        state.show_agents();

        assert!(handle_agent_workspace_key(
            KeyCode::Char('s'),
            &mut state,
            &tx,
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::StopTask { task_id }) if task_id == "child"
        ));

        let mut completed = task("completed", 2_000);
        completed.status = TaskStatus::Completed;
        state.replace_workflow_tasks_for_test(vec![completed]);
        assert!(handle_agent_workspace_key(
            KeyCode::Char('s'),
            &mut state,
            &tx,
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn workflow_agent_rows_do_not_emit_unsupported_task_actions() {
        let mut workflow = task("workflow", 1_000);
        workflow.task_type = TaskType::Workflow;
        workflow.publication_revision = None;
        workflow.workflow_agents = vec![WorkflowAgentTaskSummary {
            call_id: "child".to_string(),
            call_path: "root:child".to_string(),
            team: Some("review".to_string()),
            status: WorkflowAgentStatus::Running,
            attempt: 1,
            max_attempts: 1,
            previous_errors: Vec::new(),
            error: None,
            transcript_path: Some("/tmp/must-not-read-directly.jsonl".to_string()),
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            usage: None,
            continuation: None,
        }];
        let (mut state, rx) = state(vec![workflow]);
        let tx = state.event_tx.clone();

        assert!(handle_agent_workspace_key(KeyCode::Enter, &mut state, &tx));
        assert!(handle_agent_workspace_key(
            KeyCode::Char('s'),
            &mut state,
            &tx,
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resumable_subagent_rows_emit_resume_and_retry_actions() {
        let mut stopped = task("stopped-child", 1_000);
        stopped.status = TaskStatus::Stopped;
        stopped.continuation = Some(orca_core::task_types::TaskContinuationSummary {
            continuation_id: "continuation-1".to_string(),
            attempt_id: "attempt-1".to_string(),
            checkpoint_id: Some("checkpoint-1".to_string()),
            revision: 2,
            resumable: true,
            indeterminate: false,
        });
        let (mut state, rx) = state(vec![stopped]);
        let tx = state.event_tx.clone();

        assert!(handle_agent_workspace_key(
            KeyCode::Char('r'),
            &mut state,
            &tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ResumeTask { task_id }) if task_id == "stopped-child"
        ));

        assert!(handle_agent_workspace_key(
            KeyCode::Char('R'),
            &mut state,
            &tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::RetryTask { task_id }) if task_id == "stopped-child"
        ));
    }

    #[test]
    fn handler_ignores_keys_outside_the_agent_workspace() {
        let (mut state, _rx) = state(vec![task("child", 1_000)]);
        state.panel_mode = PanelMode::Conversation;
        let tx = state.event_tx.clone();

        assert!(!handle_agent_workspace_key(KeyCode::Down, &mut state, &tx));
    }

    #[test]
    fn open_transcript_owns_navigation_and_blocks_agent_controls() {
        let (mut state, rx) = state(vec![task("child", 1_000)]);
        let request = TaskTranscriptRequest {
            task_id: "child".to_string(),
            expected_revision: 7,
        };
        state.begin_task_transcript_request(request.clone());
        state.update(crate::protocol::TuiEvent::TaskTranscriptResult {
            request,
            result: crate::protocol::TaskTranscriptResult::unavailable("not ready"),
        });
        let tx = state.event_tx.clone();

        assert!(handle_agent_workspace_key(KeyCode::Down, &mut state, &tx));
        assert_eq!(state.task_transcript_scroll(), 1);
        assert!(handle_agent_workspace_key(KeyCode::Up, &mut state, &tx));
        assert_eq!(state.task_transcript_scroll(), 0);
        assert!(handle_agent_workspace_key(
            KeyCode::Char('s'),
            &mut state,
            &tx,
        ));
        assert!(rx.try_recv().is_err());
    }
}
