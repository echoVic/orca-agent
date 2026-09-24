use crossbeam_channel as mpsc;
use crossterm::event::KeyCode;
use orca_core::task_types::{TaskStatus, TaskType};

use crate::agent_workspace::AgentWorkspaceRow;
use crate::protocol::{TaskTranscriptRequest, UserAction};
use crate::types::{AppState, PanelMode};

/// Opens a subagent the way Enter on it does: a child on a thread of this
/// process takes over the conversation view, any other one opens its
/// transcript in the Agents panel. `false` when there is nothing to open yet,
/// because the surface has not published the task.
pub(crate) fn open_agent_task(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    task_id: &str,
) -> bool {
    let Some(task) = state
        .workflow_tasks()
        .iter()
        .find(|task| task.id == task_id && task.task_type == TaskType::Subagent)
    else {
        return false;
    };
    let Some(expected_revision) = task.publication_revision else {
        return false;
    };
    if task.subagent_child_thread_id.is_some() {
        let _ = action_tx.send(UserAction::FocusChildThread {
            task_id: task_id.to_string(),
            expected_revision,
        });
        return true;
    }
    let request = TaskTranscriptRequest {
        task_id: task_id.to_string(),
        expected_revision,
    };
    state.show_agents();
    state.select_agent_workspace_task(task_id);
    state.begin_task_transcript_request(request.clone());
    let _ = action_tx.send(UserAction::ReadTaskTranscript(request));
    true
}

/// Leaves whatever agent is open for the main conversation: a focused child
/// thread hands the view back to its parent and the Agents panel closes.
pub(crate) fn return_to_main(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>) {
    if state.conversation_target().task_id().is_some() {
        let _ = action_tx.send(UserAction::ReturnToParentThread);
    }
    if state.panel_mode == PanelMode::Agents {
        state.clear_task_transcript();
        state.show_conversation();
    }
    state.agent_dock_selected_task_id = None;
}

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
            if let Some(AgentWorkspaceRow::Subagent { task, .. }) = state.selected_agent_row() {
                let task_id = task.id.clone();
                open_agent_task(state, action_tx, &task_id);
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
                Some(AgentWorkspaceRow::WorkflowAgent { .. })
                | Some(AgentWorkspaceRow::Subagent { .. })
                | Some(AgentWorkspaceRow::BackgroundTask { .. }) => None,
                None => None,
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
                AgentWorkspaceRow::BackgroundTask { .. }
                | AgentWorkspaceRow::WorkflowAgent { .. }
                | AgentWorkspaceRow::Subagent { .. } => None,
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
    use orca_core::task_types::TaskLifetime;
    use orca_core::task_types::{
        BackgroundTaskSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    };
    use orca_core::workflow_types::WorkflowAgentStatus;

    use super::{handle_agent_workspace_key, open_agent_task, return_to_main};
    use crate::agent_workspace::AgentHitTarget;
    use crate::input_event_actions::MouseFlow;
    use crate::protocol::{TaskTranscriptRequest, UserAction};
    use crate::types::{AppState, PanelMode};

    fn task(id: &str, created_at_ms: i64) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type: TaskType::Subagent,
            status: TaskStatus::Running,
            is_backgrounded: false,
            lifetime: TaskLifetime::Task,
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
    fn enter_focuses_a_live_child_instead_of_opening_a_static_transcript() {
        let (mut state, rx) = state(vec![{
            let mut task = task("child", 1_000);
            task.subagent_child_thread_id = Some("thread-child".to_string());
            task
        }]);

        let action_tx = state.event_tx.clone();
        assert!(handle_agent_workspace_key(
            KeyCode::Enter,
            &mut state,
            &action_tx,
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision: 7,
            }) if task_id == "child"
        ));
        assert!(state.task_transcript().is_none());
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

    /// Draws one frame, as the render loop does, so the click targets are
    /// the ones on screen.
    fn render_once(state: &mut AppState) {
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = tui_textarea::TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");
        terminal
            .draw(|frame| crate::ui::render(frame, state, &textarea, &theme))
            .expect("draw");
    }

    fn clicks_on(state: &AppState, target: &AgentHitTarget) -> Vec<(u16, u16)> {
        state
            .agent_hit_areas
            .iter()
            .filter(|hit| hit.target == *target)
            .map(|hit| (hit.rect.x + 2, hit.rect.y))
            .collect()
    }

    fn click(state: &mut AppState, (column, row): (u16, u16)) -> MouseFlow {
        let mut textarea = tui_textarea::TextArea::default();
        crate::input_event_actions::handle_mouse_event(
            &crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            state,
            &mut textarea,
            std::time::Instant::now(),
        )
    }

    fn conversation_with(
        tasks: Vec<BackgroundTaskSummary>,
    ) -> (AppState, mpsc::Receiver<UserAction>) {
        let (mut state, rx) = state(tasks);
        state.show_conversation();
        (state, rx)
    }

    fn with_ready_recap(state: &mut AppState, text: &str) {
        let attachment = crate::protocol::SessionAttachmentId::new(1);
        state.active_session_attachment = Some(attachment);
        let cursor = crate::surface_projection::test_surface_cursor(3);
        state.recap = crate::types::RecapState::Ready {
            attachment,
            source: orca_runtime::recap::RecapSourceFence {
                marker: orca_runtime::recap::RecapContentMarker {
                    thread_id: cursor.thread_id.clone(),
                    incarnation: cursor.incarnation.clone(),
                    completed_user_operations: 3,
                    evidence_digest: orca_runtime::surface::Sha256Digest::digest("dock"),
                },
                cursor,
            },
            text: text.to_string(),
            usage: orca_runtime::recap::RecapUsage::Cached,
            detail_open: false,
        };
    }

    fn render_sized(state: &mut AppState, width: u16, height: u16) {
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = tui_textarea::TextArea::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test backend");
        terminal
            .draw(|frame| crate::ui::render(frame, state, &textarea, &theme))
            .expect("draw");
    }

    #[test]
    fn a_recap_under_the_agents_dock_takes_its_own_rows_and_leaves_theirs_alone() {
        let (mut state, _rx) = conversation_with(vec![task("worker", 1_000)]);
        with_ready_recap(
            &mut state,
            "Fixed the relay drain; the release tag is next.",
        );
        render_once(&mut state);

        let agent_rows = clicks_on(&state, &AgentHitTarget::DockAgent("worker".to_string()));
        assert_eq!(agent_rows.len(), 2);
        let strip = state.viewport.recap_strip_area.expect("recap strip");
        assert!(
            agent_rows.iter().all(|(_, row)| *row < strip.y),
            "the recap follows the dock: {agent_rows:?} {strip:?}"
        );
        assert!(
            state
                .agent_hit_areas
                .iter()
                .all(|hit| hit.rect.intersection(strip).is_empty()),
            "no agent target under the recap"
        );
        for at in &agent_rows {
            assert_eq!(
                click(&mut state, *at),
                MouseFlow::OpenAgent("worker".to_string())
            );
        }
        assert!(!state.recap_detail_open());
        assert_eq!(
            click(&mut state, (strip.x + 3, strip.y)),
            MouseFlow::Handled
        );
        assert!(state.recap_detail_open());
    }

    #[test]
    fn a_short_terminal_drops_the_recap_before_any_agent_row() {
        let (mut state, _rx) = conversation_with(vec![task("worker", 1_000)]);
        with_ready_recap(
            &mut state,
            "Fixed the relay drain; the release tag is next.",
        );
        render_sized(&mut state, 100, 30);
        let agent_rows = clicks_on(&state, &AgentHitTarget::DockAgent("worker".to_string()));
        assert!(state.viewport.recap_strip_area.is_some());

        // Shrink until the recap no longer fits: the agent rows stay, and at
        // no height does the recap's click target cover one of theirs.
        let mut height = 30;
        while let Some(strip) = state.viewport.recap_strip_area {
            assert!(
                state
                    .agent_hit_areas
                    .iter()
                    .all(|hit| hit.rect.intersection(strip).is_empty()),
                "at {height} rows"
            );
            height -= 1;
            render_sized(&mut state, 100, height);
        }
        assert_eq!(
            clicks_on(&state, &AgentHitTarget::DockAgent("worker".to_string())).len(),
            agent_rows.len(),
            "at {height} rows"
        );
    }

    #[test]
    fn a_click_on_a_running_agent_in_the_dock_opens_its_transcript() {
        let (mut state, rx) = conversation_with(vec![task("worker", 1_000)]);
        render_once(&mut state);
        let rows = clicks_on(&state, &AgentHitTarget::DockAgent("worker".to_string()));
        assert_eq!(rows.len(), 2, "its name row and its detail row");

        for at in &rows {
            assert_eq!(
                click(&mut state, *at),
                MouseFlow::OpenAgent("worker".to_string())
            );
        }
        assert_eq!(state.agent_dock_selected_task_id.as_deref(), Some("worker"));

        let tx = state.event_tx.clone();
        assert!(open_agent_task(&mut state, &tx, "worker"));
        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert_eq!(
            state
                .task_transcript()
                .map(|view| view.request.task_id.as_str()),
            Some("worker")
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ReadTaskTranscript(TaskTranscriptRequest {
                task_id,
                expected_revision: 7,
            })) if task_id == "worker"
        ));
    }

    #[test]
    fn an_agent_on_a_live_child_thread_opens_by_focusing_that_thread() {
        let mut child = task("child", 1_000);
        child.subagent_child_thread_id = Some("child-thread".to_string());
        let (mut state, rx) = conversation_with(vec![child]);
        let tx = state.event_tx.clone();

        assert!(open_agent_task(&mut state, &tx, "child"));

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision: 7,
            }) if task_id == "child"
        ));
        let mut unpublished = task("unpublished", 2_000);
        unpublished.publication_revision = None;
        state.replace_workflow_tasks_for_test(vec![unpublished]);
        assert!(
            !open_agent_task(&mut state, &tx, "unpublished"),
            "an agent the surface has not published has nothing to open"
        );
    }

    #[test]
    fn a_click_on_main_leaves_the_open_agent_for_the_main_conversation() {
        let (mut state, rx) = conversation_with(vec![task("worker", 1_000)]);
        let tx = state.event_tx.clone();
        assert!(open_agent_task(&mut state, &tx, "worker"));
        let _ = rx.try_recv();
        render_once(&mut state);

        let main = clicks_on(&state, &AgentHitTarget::Main);
        assert_eq!(main.len(), 1);
        assert_eq!(click(&mut state, main[0]), MouseFlow::ReturnToMain);
        return_to_main(&mut state, &tx);

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(state.task_transcript().is_none());
        assert!(state.agent_dock_selected_task_id.is_none());
        assert!(rx.try_recv().is_err(), "no child thread was focused");

        state.update(crate::protocol::TuiEvent::ChildFocusChanged {
            task_id: Some("worker".to_string()),
        });
        return_to_main(&mut state, &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ReturnToParentThread)
        ));
    }

    #[test]
    fn a_click_on_the_dock_header_opens_the_agents_panel() {
        let (mut state, _rx) = conversation_with(vec![task("worker", 1_000)]);
        render_once(&mut state);
        let header = clicks_on(&state, &AgentHitTarget::Workspace);

        assert_eq!(click(&mut state, header[0]), MouseFlow::Handled);
        assert_eq!(state.panel_mode, PanelMode::Agents);
    }

    #[test]
    fn a_panel_row_is_selected_by_one_click_and_opened_by_the_next() {
        let (mut state, _rx) = state(vec![task("first", 1_000), task("second", 2_000)]);
        render_once(&mut state);
        let second = clicks_on(&state, &AgentHitTarget::WorkspaceRow(1));
        assert_eq!(second.len(), 1);

        assert_eq!(click(&mut state, second[0]), MouseFlow::Handled);
        assert_eq!(state.agent_selected_index(), 1);
        render_once(&mut state);
        assert_eq!(
            click(&mut state, second[0]),
            MouseFlow::OpenAgent("second".to_string())
        );
    }

    #[test]
    fn the_shortcuts_overlay_keeps_the_dock_from_taking_a_click() {
        let (mut state, _rx) = conversation_with(vec![task("worker", 1_000)]);
        render_once(&mut state);
        let rows = clicks_on(&state, &AgentHitTarget::DockAgent("worker".to_string()));
        state.show_shortcuts = true;

        assert_ne!(
            click(&mut state, rows[0]),
            MouseFlow::OpenAgent("worker".to_string())
        );
    }
}
