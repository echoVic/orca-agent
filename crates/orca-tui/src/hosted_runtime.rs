use std::io;

use orca_runtime::runtime_host::{HostedOperationKind, HostedTurnRequest, RuntimeThreadHandle};

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) enum TuiHostedOperationOutcome {
    Turn { status: String },
    ManualCompaction,
}

pub(crate) fn run_hosted_ordinary_turn(
    config: &orca_core::config::RunConfig,
    thread: &RuntimeThreadHandle,
    request: HostedTurnRequest,
    event_tx: &crossbeam_channel::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) -> io::Result<TuiHostedOperationOutcome> {
    TuiSurfaceActions::new(thread.typed_surface()).run_turn(
        request,
        config.clone(),
        control,
        event_tx,
    )
}

pub(crate) fn hosted_turn_request(
    submitted_turn: &SubmittedTurn,
    goal_mode_active: bool,
) -> HostedTurnRequest {
    HostedTurnRequest::new(submitted_turn.prompt().to_string())
        .with_images(submitted_turn.images().to_vec())
        .with_goal_tools(goal_mode_active)
        .with_goal_usage_tracking(goal_mode_active)
        .with_backtrack_target(submitted_turn.is_backtrack_target())
        .with_task_description(
            submitted_turn
                .task_label()
                .unwrap_or_else(|| submitted_turn.prompt()),
        )
}

pub(crate) fn send_hosted_operation_terminal_failure(
    event_tx: &crossbeam_channel::Sender<TuiEvent>,
    _operation_kind: &HostedOperationKind,
) {
    let _ = event_tx.send(TuiEvent::SessionCompleted {
        status: "failed".to_string(),
    });
}

pub(crate) fn emit_hosted_operation_error(
    event_tx: &crossbeam_channel::Sender<TuiEvent>,
    error: io::Error,
    operation_kind: &HostedOperationKind,
) {
    let recovery_required = crate::surface_client::is_terminal_recovery_error(&error);
    let _ = event_tx.send(TuiEvent::Error(error.to_string()));
    if !recovery_required {
        send_hosted_operation_terminal_failure(event_tx, operation_kind);
    }
}

#[cfg(test)]
pub(crate) fn send_submission_error(
    event_tx: &crossbeam_channel::Sender<TuiEvent>,
    queued_id: Option<u64>,
    rejection_prompt: Option<&str>,
    message: String,
) {
    send_submission_error_with_images(
        event_tx,
        queued_id,
        rejection_prompt,
        orca_runtime::mentions::MentionBindings::default(),
        Vec::new(),
        message,
    );
}

pub(crate) fn send_submission_error_with_images(
    event_tx: &crossbeam_channel::Sender<TuiEvent>,
    queued_id: Option<u64>,
    rejection_prompt: Option<&str>,
    bindings: orca_runtime::mentions::MentionBindings,
    images: Vec<crate::composer_images::ComposerImageAttachment>,
    message: String,
) {
    if let Some(prompt) = rejection_prompt {
        let _ = event_tx.send(TuiEvent::SubmissionRejected {
            queued_id,
            prompt: prompt.to_string(),
            bindings,
            images,
            message,
        });
    } else {
        let _ = event_tx.send(TuiEvent::Error(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submitted_turn::SubmittedTurn;

    #[test]
    fn hosted_turn_request_preserves_goal_and_task_flags() {
        let submitted = SubmittedTurn::user("inspect the runtime".to_string());
        let request = hosted_turn_request(&submitted, true);

        assert!(request.allows_goal_tools());
        assert!(request.tracks_goal_usage());
        assert!(request.is_backtrack_target());
        assert_eq!(request.task_description(), Some("inspect the runtime"));
    }

    #[test]
    fn hosted_turn_request_preserves_pinned_workflow_notification_semantics() {
        let submitted =
            SubmittedTurn::workflow_notification(crate::protocol::PendingWorkflowNotification {
                id: "notification-42".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
            });
        let request = hosted_turn_request(&submitted, true);

        assert!(request.allows_goal_tools());
        assert!(request.tracks_goal_usage());
        assert!(!request.is_backtrack_target());
        assert_eq!(
            request.task_description(),
            Some("Workflow notification notification-42")
        );
    }

    #[test]
    fn hosted_operation_error_shapes_non_recovery_terminal() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        emit_hosted_operation_error(
            &event_tx,
            io::Error::other("operation failed"),
            &HostedOperationKind::Turn,
        );

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            events.as_slice(),
            [TuiEvent::Error(message), TuiEvent::SessionCompleted { status }]
                if message == "operation failed" && status == "failed"
        ));
    }
}
