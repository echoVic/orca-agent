use crossbeam_channel as mpsc;

use orca_core::task_types::TaskStatus;
use orca_runtime::runtime_host::RuntimeThreadHandle;

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) enum HostedTaskAction {
    Stop { task_id: String },
    Foreground { task_id: String },
    ResolveBackgroundApproval { id: String, approved: bool },
}

pub(crate) fn handle_hosted_task_action(
    action: HostedTaskAction,
    thread: Option<&RuntimeThreadHandle>,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let actions = thread.map(|thread| TuiSurfaceActions::new(thread.typed_surface()));
    match action {
        HostedTaskAction::Stop { task_id } => {
            let _ = stop_task_for_tui(actions.as_ref(), &task_id, control, event_tx);
        }
        HostedTaskAction::Foreground { task_id } => {
            let _ = foreground_task_for_tui(actions.as_ref(), &task_id, control, event_tx);
        }
        HostedTaskAction::ResolveBackgroundApproval { id, approved } => {
            let resolved = submit_background_approval_response_for_tui(
                actions.as_ref(),
                &id,
                approved,
                control,
                event_tx,
            );
            if !approved || !resolved {
                control.cancel_surface_activation();
            }
        }
    }
}

fn stop_task_for_tui(
    actions: Option<&TuiSurfaceActions>,
    task_id: &str,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot stop task before a session exists".to_string(),
        ));
        return false;
    };
    match actions.stop_task(task_id, control, event_tx) {
        Ok(projection) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Task stop requested for {task_id}."
            )));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
        }
    }
}

fn foreground_task_for_tui(
    actions: Option<&TuiSurfaceActions>,
    task_id: &str,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot foreground task before a session exists".to_string(),
        ));
        return false;
    };

    match actions.foreground_task(task_id, control, event_tx) {
        Ok(projection) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Task {task_id} returned to foreground."
            )));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
        }
    }
}

fn submit_background_approval_response_for_tui(
    actions: Option<&TuiSurfaceActions>,
    approval_id: &str,
    approved: bool,
    control: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot resolve background approval before a session exists".to_string(),
        ));
        return false;
    };

    match actions.resolve_background_approval(approval_id, approved, control, event_tx) {
        Ok((task_id, projection)) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let decision = if approved { "approved" } else { "denied" };
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Background approval {decision} for {task_id}."
            )));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
        }
    }
}

pub(crate) fn notify_recovered_background_approvals_for_tui(
    actions: &TuiSurfaceActions,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> usize {
    let Ok((projection, recovered_tools)) = actions.recoverable_background_approval_projection()
    else {
        return 0;
    };

    if recovered_tools.is_empty() {
        return 0;
    }

    let count = recovered_tools.len();
    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
    let summary = if count == 1 {
        format!(
            "Recovered background session waiting for approval for {}.",
            recovered_tools[0]
        )
    } else {
        format!(
            "Recovered {count} background sessions waiting for approval: {}.",
            recovered_tools.join(", ")
        )
    };
    let _ = event_tx.send(TuiEvent::Notice(summary));
    count
}

pub(crate) fn is_terminal_task_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Stopped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_thread_background_approval_releases_prearmed_activation() {
        let control = TuiSurfaceTaskControl::isolated_for_test();
        assert!(
            control
                .begin_surface_activation()
                .expect("prearm background approval")
        );
        let (event_tx, event_rx) = mpsc::unbounded();

        handle_hosted_task_action(
            HostedTaskAction::ResolveBackgroundApproval {
                id: "approval-missing".to_string(),
                approved: false,
            },
            None,
            &control,
            &event_tx,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message))
                if message == "cannot resolve background approval before a session exists"
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(
            control
                .begin_surface_activation()
                .expect("failed approval releases activation")
        );
        control.cancel_surface_activation();
    }

    #[test]
    fn missing_thread_task_controls_preserve_exact_errors() {
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        handle_hosted_task_action(
            HostedTaskAction::Stop {
                task_id: "task-stop".to_string(),
            },
            None,
            &control,
            &event_tx,
        );
        handle_hosted_task_action(
            HostedTaskAction::Foreground {
                task_id: "task-foreground".to_string(),
            },
            None,
            &control,
            &event_tx,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message))
                if message == "cannot stop task before a session exists"
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message))
                if message == "cannot foreground task before a session exists"
        ));
        assert!(event_rx.try_recv().is_err());
    }
}
