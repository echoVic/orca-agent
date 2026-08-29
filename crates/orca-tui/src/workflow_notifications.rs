use crossbeam_channel as mpsc;

use crate::bridge;
use crate::protocol::{PendingWorkflowNotification, TuiEvent, UserAction};
use crate::types::{AppState, AppStatus};

pub(crate) fn submit_pending_workflow_notification(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    require_idle: bool,
) {
    if require_idle && state.status != AppStatus::Idle {
        return;
    }
    if let Some(notification) = state.pending_workflow_notifications.pop_front() {
        state.enter_running();
        state.scroll_to_bottom();
        let _ = action_tx.send(UserAction::SubmitWorkflowNotification(notification));
    }
}

pub(crate) fn queue_workflow_terminal_notification(
    event: &TuiEvent,
    pending_notifications: &bridge::PendingWorkflowNotifications,
    batch_injection_enabled: bool,
) -> Option<String> {
    if !batch_injection_enabled {
        return None;
    }
    if let TuiEvent::WorkflowNotification { id, prompt, .. } = event {
        let queued = pending_notifications.push_unique(PendingWorkflowNotification {
            id: id.clone(),
            prompt: prompt.clone(),
        });
        if queued {
            return Some(id.clone());
        }
    }
    None
}

pub(crate) fn remove_pending_workflow_notification_by_id(state: &mut AppState, id: &str) {
    if let Some(index) = state
        .pending_workflow_notifications
        .iter()
        .position(|pending| pending.id == id)
    {
        state.pending_workflow_notifications.remove(index);
    }
}

pub(crate) fn drain_pending_workflow_notifications(
    state: &mut AppState,
    pending_notifications: &bridge::PendingWorkflowNotifications,
) {
    pending_notifications.drain_into(&mut state.pending_workflow_notifications);
}

pub(crate) fn is_workflow_notification_turn_boundary(event: &TuiEvent) -> bool {
    matches!(event, TuiEvent::SessionCompleted { .. })
}
