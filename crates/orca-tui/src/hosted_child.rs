//! Live hosted child conversation focus and attachment fencing.
//!
//! A child focus is deliberately resolved from the parent's typed surface on
//! every request.  The TUI never accepts a child thread id from an action, so
//! a stale row cannot redirect the session to an unrelated runtime thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};
use orca_runtime::surface::{SurfaceSnapshot, SurfaceTaskId};

use crate::attachment_routing::{AttachmentRouting, spawn_attached_event_sender_with_routing};
use crate::hosted_session::{
    announce_runtime_ready, project_hosted_thread_attached, read_hosted_projection_batch,
};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TuiEvent};

/// A controller-owned attachment pair for an interactive child.
///
/// The child handle is borrowed from the runtime host's live-thread table. It
/// is not reconstructed from a saved-session selector and is not owned by the
/// TUI transcript. The parent remains available so its exact attachment and
/// event sender can be restored on return.
pub(crate) struct HostedChildFocus {
    pub(crate) parent_thread: RuntimeThreadHandle,
    pub(crate) parent_event_tx: mpsc::Sender<TuiEvent>,
    pub(crate) parent_attachment: SessionAttachmentId,
    pub(crate) child_thread: RuntimeThreadHandle,
    #[allow(dead_code)]
    pub(crate) child_event_tx: mpsc::Sender<TuiEvent>,
    #[allow(dead_code)]
    pub(crate) child_attachment: SessionAttachmentId,
    #[allow(dead_code)]
    pub(crate) task_id: String,
}

pub(crate) enum HostedChildAction {
    Focus {
        task_id: String,
        expected_revision: u64,
    },
    Return,
}

/// Handle a typed live-child action and update the controller's active thread
/// and attachment sender. This function is intentionally the only place where
/// the runtime-owned child thread id is resolved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_child_action(
    action: HostedChildAction,
    thread: &mut Option<RuntimeThreadHandle>,
    focus: &mut Option<HostedChildFocus>,
    side_active: bool,
    host: &RuntimeHostHandle,
    root_event_tx: &mpsc::Sender<TuiEvent>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    attachment_routing: &Arc<Mutex<AttachmentRouting>>,
    control: &TuiSurfaceTaskControl,
) {
    match action {
        HostedChildAction::Focus {
            task_id,
            expected_revision,
        } => focus_child(
            task_id,
            expected_revision,
            thread,
            focus,
            side_active,
            host,
            root_event_tx,
            event_tx,
            session_attachment,
            attachment_routing,
            control,
        ),
        HostedChildAction::Return => return_to_parent(
            thread,
            focus,
            root_event_tx,
            event_tx,
            session_attachment,
            attachment_routing,
            control,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn focus_child(
    task_id: String,
    expected_revision: u64,
    thread: &mut Option<RuntimeThreadHandle>,
    focus: &mut Option<HostedChildFocus>,
    side_active: bool,
    host: &RuntimeHostHandle,
    root_event_tx: &mpsc::Sender<TuiEvent>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    attachment_routing: &Arc<Mutex<AttachmentRouting>>,
    control: &TuiSurfaceTaskControl,
) {
    if side_active {
        reject(
            event_tx,
            "live child focus is unavailable in a side conversation",
        );
        return;
    }
    if focus.is_some() {
        reject(event_tx, "a live child conversation is already focused");
        return;
    }
    let Some(parent_thread) = thread.as_ref().cloned() else {
        reject(
            event_tx,
            "focus a main conversation before selecting a child",
        );
        return;
    };

    let parent_snapshot = match read_parent_snapshot(&parent_thread) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            reject(event_tx, &format!("failed to read parent surface: {error}"));
            return;
        }
    };
    let binding = match child_binding(&parent_snapshot, &task_id, expected_revision) {
        Ok(binding) => binding,
        Err(error) => {
            reject(event_tx, &error);
            return;
        }
    };

    // Resolve only a runtime-hosted live thread. A saved-session resume here
    // would let an old row redirect the user to a different conversation and
    // would violate the parent/child ownership fence.
    let child_thread = match host.resolve_live_thread(&binding.child_thread_id) {
        Ok(child) => child,
        Err(_) => {
            reject(
                event_tx,
                "the selected child no longer has a live conversation",
            );
            return;
        }
    };
    if child_thread.parent_thread_id() != Some(parent_thread.thread_id()) {
        reject(
            event_tx,
            "the selected child is not owned by this conversation",
        );
        return;
    }

    // The host lookup can race the child terminal commit. Re-read the parent
    // surface and require both the task revision and bound thread to remain
    // identical before changing the visible attachment.
    let current_snapshot = match read_parent_snapshot(&parent_thread) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            reject(
                event_tx,
                &format!("failed to recheck parent surface: {error}"),
            );
            return;
        }
    };
    let current_binding = match child_binding(&current_snapshot, &task_id, expected_revision) {
        Ok(binding) => binding,
        Err(error) => {
            reject(event_tx, &format!("child focus became stale: {error}"));
            return;
        }
    };
    if current_binding.child_thread_id != binding.child_thread_id {
        reject(
            event_tx,
            "child focus became stale: the bound conversation changed",
        );
        return;
    }

    let child_batch = match read_hosted_projection_batch(&child_thread) {
        Ok(batch) => batch,
        Err(error) => {
            reject(
                event_tx,
                &format!("failed to read child conversation: {error}"),
            );
            return;
        }
    };

    let parent_attachment = *session_attachment;
    let child_attachment = parent_attachment.next();
    let parent_event_tx = event_tx.clone();
    let child_event_tx = spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        child_attachment,
        Some(attachment_routing.clone()),
    );

    AttachmentRouting::switch_attachment(
        attachment_routing,
        root_event_tx,
        child_attachment,
        Some(parent_attachment),
        false,
    );
    *thread = Some(child_thread.clone());
    *event_tx = child_event_tx.clone();
    *session_attachment = child_attachment;
    *focus = Some(HostedChildFocus {
        parent_thread,
        parent_event_tx,
        parent_attachment,
        child_thread: child_thread.clone(),
        child_event_tx,
        child_attachment,
        task_id: task_id.clone(),
    });

    if let Err(error) = project_hosted_thread_attached(
        child_batch.0,
        child_batch.1,
        child_attachment,
        root_event_tx,
    ) {
        // Projection is the commit point for a focus transition. If the
        // child snapshot cannot be installed, restore the parent attachment
        // and leave the controller's active-thread state untouched.
        AttachmentRouting::switch_attachment(
            attachment_routing,
            root_event_tx,
            parent_attachment,
            None,
            true,
        );
        *thread = Some(
            focus
                .as_ref()
                .expect("focus record after child projection failure")
                .parent_thread
                .clone(),
        );
        *event_tx = focus
            .as_ref()
            .expect("focus record after child projection failure")
            .parent_event_tx
            .clone();
        *session_attachment = parent_attachment;
        *focus = None;
        reject(
            event_tx,
            &format!("failed to project child conversation: {error}"),
        );
        return;
    }
    announce_runtime_ready(&child_thread, event_tx, control);
    let _ = event_tx.send(TuiEvent::ChildFocusChanged {
        task_id: Some(task_id),
    });
}

#[allow(clippy::too_many_arguments)]
fn return_to_parent(
    thread: &mut Option<RuntimeThreadHandle>,
    focus: &mut Option<HostedChildFocus>,
    root_event_tx: &mpsc::Sender<TuiEvent>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    attachment_routing: &Arc<Mutex<AttachmentRouting>>,
    control: &TuiSurfaceTaskControl,
) {
    let Some(child_focus) = focus.take() else {
        return;
    };
    let parent_batch = match read_hosted_projection_batch(&child_focus.parent_thread) {
        Ok(batch) => batch,
        Err(error) => {
            reject(
                event_tx,
                &format!("failed to return to parent conversation: {error}"),
            );
            *focus = Some(child_focus);
            return;
        }
    };

    // Install the parent attachment first, but hold parent events until its
    // reset/history projection is visible. This is the same barrier used by
    // side conversations and prevents a late child event from mutating the
    // newly restored parent view.
    AttachmentRouting::switch_attachment_deferred(
        attachment_routing,
        root_event_tx,
        child_focus.parent_attachment,
        None,
    );
    *thread = Some(child_focus.parent_thread.clone());
    *event_tx = child_focus.parent_event_tx.clone();
    *session_attachment = child_focus.parent_attachment;

    if let Err(error) = project_hosted_thread_attached(
        parent_batch.0,
        parent_batch.1,
        child_focus.parent_attachment,
        root_event_tx,
    ) {
        reject(
            event_tx,
            &format!("failed to project parent conversation: {error}"),
        );
        // The parent attachment is already active. Keeping the focus record
        // lets a subsequent return retry without accepting a raw thread id.
        *focus = Some(child_focus);
        return;
    }
    AttachmentRouting::release_deferred_parent_events(attachment_routing, root_event_tx);
    announce_runtime_ready(
        thread.as_ref().expect("parent thread after child return"),
        event_tx,
        control,
    );
    let _ = event_tx.send(TuiEvent::ChildFocusChanged { task_id: None });
}

fn reject(event_tx: &mpsc::Sender<TuiEvent>, message: &str) {
    let _ = event_tx.send(TuiEvent::OperationRejected(message.to_string()));
}

fn read_parent_snapshot(thread: &RuntimeThreadHandle) -> Result<SurfaceSnapshot, String> {
    crate::surface_actions::TuiSurfaceActions::new(thread.typed_surface())
        .read_snapshot()
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChildBinding {
    child_thread_id: String,
}

fn child_binding(
    snapshot: &SurfaceSnapshot,
    task_id: &str,
    expected_revision: u64,
) -> Result<ChildBinding, String> {
    let task_id = SurfaceTaskId::try_new(task_id.to_string())
        .map_err(|_| "invalid child task identity".to_string())?;
    let task = snapshot
        .tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| "the selected child task is no longer present".to_string())?;
    if task.revision.get() != expected_revision {
        return Err(format!(
            "the selected child task is stale (expected revision {expected_revision})"
        ));
    }
    let subagent_id = task
        .subagent_id
        .as_ref()
        .ok_or_else(|| "the selected task is not a delegated child".to_string())?;
    let subagent = snapshot
        .subagents
        .iter()
        .find(|subagent| &subagent.subagent_id == subagent_id)
        .ok_or_else(|| "the selected child has no surface binding".to_string())?;
    let child_thread_id = subagent
        .child_thread_id
        .as_ref()
        .ok_or_else(|| "the selected child has no live conversation".to_string())?;
    Ok(ChildBinding {
        child_thread_id: uuid::Uuid::from_bytes(*child_thread_id.as_bytes()).to_string(),
    })
}

/// The focused child is runtime-owned; the controller only has a borrowed
/// handle. Shutdown still settles the active child before the parent when the
/// TUI itself exits, matching the side-conversation lifecycle contract.
pub(crate) fn shutdown_attached_child_on_controller_exit(focus: HostedChildFocus) {
    let _ = focus
        .child_thread
        .shutdown_with_timeout(Duration::from_secs(5));
    let _ = focus
        .parent_thread
        .shutdown_with_timeout(Duration::from_secs(5));
}

#[cfg(test)]
mod tests {
    use super::ChildBinding;

    #[test]
    fn binding_identity_is_only_the_runtime_resolved_child_id() {
        let left = ChildBinding {
            child_thread_id: "child-a".to_string(),
        };
        let right = ChildBinding {
            child_thread_id: "child-a".to_string(),
        };
        assert_eq!(left, right);
    }
}
