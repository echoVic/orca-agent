use crossbeam_channel as mpsc;

use crossterm::event::KeyCode;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};

use crate::background_tasks::is_terminal_task_status;
use crate::protocol::UserAction;
use crate::types::{AppState, PanelMode};

pub(crate) fn handle_workflows_panel_key(
    key_code: KeyCode,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    if state.panel_mode != PanelMode::Workflows {
        return false;
    }

    match key_code {
        KeyCode::Up => {
            state.select_previous_workflow_task();
            true
        }
        KeyCode::Down => {
            state.select_next_workflow_task();
            true
        }
        KeyCode::Enter => state.open_selected_background_approval_dialog(),
        KeyCode::Char('s') => {
            let Some(task) = selected_stoppable_task(state) else {
                return false;
            };
            let _ = action_tx.send(UserAction::StopTask {
                task_id: task.id.clone(),
            });
            true
        }
        KeyCode::Char('f') => {
            let Some(task) = selected_foregroundable_task(state) else {
                return false;
            };
            let _ = action_tx.send(UserAction::ForegroundTask {
                task_id: task.id.clone(),
            });
            true
        }
        _ => false,
    }
}

fn selected_stoppable_task(state: &AppState) -> Option<&BackgroundTaskSummary> {
    let task = state.selected_workflow_task()?;
    if is_terminal_task_status(task.status) {
        return None;
    }
    Some(task)
}

fn selected_foregroundable_task(state: &AppState) -> Option<&BackgroundTaskSummary> {
    let task = state.selected_workflow_task()?;
    if task.task_type != TaskType::MainSession
        || task.status != TaskStatus::Running
        || !task.is_backgrounded
    {
        return None;
    }
    Some(task)
}
