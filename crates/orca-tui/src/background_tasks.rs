use crossbeam_channel as mpsc;

use orca_core::task_types::{BackgroundTaskSummary, TaskStatus};
use orca_runtime::runtime_host::RuntimeThreadHandle;

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) enum HostedTaskAction {
    Stop { task_id: String },
    Foreground { task_id: String },
    Resume { task_id: String },
    Retry { task_id: String },
    FollowUp { task_id: String, prompt: String },
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
        HostedTaskAction::Resume { task_id } => {
            continue_subagent_for_tui(actions.as_ref(), &task_id, "resume", None, event_tx);
        }
        HostedTaskAction::Retry { task_id } => {
            continue_subagent_for_tui(actions.as_ref(), &task_id, "retry", None, event_tx);
        }
        HostedTaskAction::FollowUp { task_id, prompt } => {
            continue_subagent_for_tui(
                actions.as_ref(),
                &task_id,
                "follow-up",
                Some(prompt.as_str()),
                event_tx,
            );
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

fn continue_subagent_for_tui(
    actions: Option<&TuiSurfaceActions>,
    task_id: &str,
    mode: &str,
    prompt: Option<&str>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else { return false };
    match actions.continue_subagent(task_id, mode, prompt) {
        Ok(projection) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let _ = event_tx.send(TuiEvent::Notice(format!("Child {mode} started.")));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
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
            let label = task_notice_label(&projection.workflow_tasks, task_id);
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let _ = event_tx.send(TuiEvent::Notice(format!("Stopping {label}.")));
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
            let label = task_notice_label(&projection.workflow_tasks, task_id);
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Brought {label} back to the foreground."
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
            let label = task_notice_label(&projection.workflow_tasks, &task_id);
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
            let notice = if approved {
                format!("Approved; {label} continues in the background.")
            } else {
                format!("Denied; {label} stops.")
            };
            let _ = event_tx.send(TuiEvent::Notice(notice));
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

/// How a notice names a task: its name, or else its description, on one
/// line and quoted. Never its id.
pub(crate) fn task_notice_label(tasks: &[BackgroundTaskSummary], task_id: &str) -> String {
    tasks
        .iter()
        .find(|task| task.id == task_id)
        .map(|task| {
            task.name
                .as_deref()
                .unwrap_or(task.description.as_str())
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|label| !label.is_empty())
        .map(|label| {
            format!(
                "\"{}\"",
                crate::display_text::truncate_to_display_width(&label, 48)
            )
        })
        .unwrap_or_else(|| "the task".to_string())
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

    fn summary(id: &str, name: Option<&str>, description: &str) -> BackgroundTaskSummary {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "type": "main_session",
            "status": "approval_required",
            "description": description,
            "createdAtMs": 1,
            "name": name,
        }))
        .expect("task summary")
    }

    #[test]
    fn a_task_notice_names_the_task_not_its_id() {
        // Notices read "Background approval approved for task-0199c1e2-…".
        let tasks = [
            summary("task-0199c1e2", None, "deploy the site\n  to staging"),
            summary("task-7a41", Some("reviewer"), "review the diff"),
        ];
        assert_eq!(
            task_notice_label(&tasks, "task-0199c1e2"),
            "\"deploy the site to staging\""
        );
        assert_eq!(task_notice_label(&tasks, "task-7a41"), "\"reviewer\"");
        assert_eq!(task_notice_label(&tasks, "task-gone"), "the task");
        let long = [summary("task-long", None, &"word ".repeat(40))];
        assert!(task_notice_label(&long, "task-long").chars().count() <= 50);
    }

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
