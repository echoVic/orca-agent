//! Live hosted child conversation focus and attachment fencing.
//!
//! A child focus is deliberately resolved from the parent's typed surface on
//! every request.  The TUI never accepts a child thread id from an action, so
//! a stale row cannot redirect the session to an unrelated runtime thread.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};
use orca_runtime::surface::{
    AttachResult, FreshAttachRequest, RuntimeSurfaceClientHandle, RuntimeSurfaceHandle,
    SurfaceCapability, SurfaceEvent, SurfaceInteractionKind, SurfaceInteractionLifecycle,
    SurfaceOperationId, SurfaceRequestId, SurfaceSnapshot, SurfaceSubscriptionItem, SurfaceTaskId,
};

use crate::attachment_routing::{
    AttachmentRouting, send_attached_event, spawn_attached_event_sender_with_routing,
};
use crate::hosted_session::{
    announce_runtime_ready, hosted_projection_batch_from_snapshot, project_hosted_thread_attached,
    read_hosted_projection_batch,
};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TuiEvent};
use crate::surface_projection::{SurfaceProjectionState, TuiSurfaceProjection};
use crate::transcript_state::ChatMessage;

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
    pub(crate) child_event_tx: mpsc::Sender<TuiEvent>,
    pub(crate) child_attachment: SessionAttachmentId,
    pub(crate) event_bridge: HostedChildEventBridge,
}

/// Passive typed-surface subscription for the operation that was visible when
/// a child was focused. The bridge owns no operation control; it only projects
/// committed child batches into the currently attached TUI sender. It exits
/// at the child operation terminal so a later child follow-up can use the
/// normal TUI operation drain without duplicate deltas.
pub(crate) struct HostedChildEventBridge {
    stop: Arc<AtomicBool>,
    started: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    control: TuiSurfaceTaskControl,
}

impl HostedChildEventBridge {
    fn prepare(
        thread: &RuntimeThreadHandle,
        task_id: String,
        event_tx: mpsc::Sender<TuiEvent>,
        control: &TuiSurfaceTaskControl,
    ) -> Result<
        (
            Self,
            SurfaceProjectionState,
            Vec<ChatMessage>,
            Vec<TuiEvent>,
        ),
        String,
    > {
        let surface = thread.surface();
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: orca_runtime::surface::SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: child_interaction_capabilities(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            AttachResult::Denied { reason } => {
                return Err(format!(
                    "child live transcript attachment denied: {reason:?}"
                ));
            }
            AttachResult::Unavailable { reason } => {
                return Err(format!(
                    "child live transcript attachment unavailable: {reason:?}"
                ));
            }
            AttachResult::ThreadClosed { .. } => {
                return Err("child live transcript thread is closed".to_string());
            }
            AttachResult::CursorAttached { .. }
            | AttachResult::SnapshotRequired { .. }
            | AttachResult::InvalidCursor { .. } => {
                return Err("child live transcript attachment was not fresh".to_string());
            }
        };
        let client = attachment.client.clone();
        let snapshot = attachment.baseline.snapshot.clone();
        let (initial_projection, initial_messages) =
            hosted_projection_batch_from_snapshot(thread, &snapshot);
        let mut projection = TuiSurfaceProjection::from_surface_snapshot(&snapshot);
        let operation_id = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.terminal.is_none())
            .map(|operation| operation.operation_id.clone());
        if let Some(operation_id) = operation_id.as_ref() {
            if let Err(error) =
                control.install_focused_child_surface(client.clone(), operation_id.clone())
            {
                detach_child_surface(&surface, &client);
                return Err(error.to_string());
            }
        }
        let initial_interactions = register_requested_interactions(&snapshot, control);
        let Some(subscription) = surface.claim_subscription(&attachment.subscription) else {
            detach_child_surface(&surface, &client);
            if let Some(operation_id) = operation_id.as_ref() {
                control.clear_focused_child_surface(operation_id);
            }
            return Err("child live transcript subscription unavailable".to_string());
        };
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_for_thread = Arc::clone(&stop);
        let started_for_thread = Arc::clone(&started);
        let surface_for_thread = surface.clone();
        let client_for_thread = client.clone();
        let control_for_thread = control.clone();
        let operation_id_for_thread = operation_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!(
                "orca-child-surface-{}",
                thread.thread_id().chars().take(8).collect::<String>()
            ))
            .spawn(move || {
                run_child_surface_bridge(
                    subscription,
                    &mut projection,
                    operation_id_for_thread,
                    event_tx,
                    stop_for_thread,
                    started_for_thread,
                    surface_for_thread,
                    client_for_thread,
                    control_for_thread,
                    task_id,
                );
            })
            .map_err(|error| {
                detach_child_surface(&surface, &client);
                format!("failed to start child live transcript bridge: {error}")
            })?;
        Ok((
            Self {
                stop,
                started,
                handle: Some(handle),
                surface,
                client,
                control: control.clone(),
            },
            initial_projection,
            initial_messages,
            initial_interactions,
        ))
    }

    fn start(&self) {
        let (started, wake) = &*self.started;
        if let Ok(mut value) = started.lock() {
            *value = true;
            wake.notify_one();
        }
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.start();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.control.clear_any_focused_child_surface();
    }
}

impl Drop for HostedChildEventBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.start();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // The worker normally detaches on exit. This is idempotent and also
        // covers a panic before the worker reaches its cleanup path.
        detach_child_surface(&self.surface, &self.client);
        self.control.clear_any_focused_child_surface();
    }
}

fn run_child_surface_bridge(
    mut subscription: orca_runtime::surface::SurfaceSubscriptionReceiver,
    projection: &mut TuiSurfaceProjection,
    operation_id: Option<SurfaceOperationId>,
    event_tx: mpsc::Sender<TuiEvent>,
    stop: Arc<AtomicBool>,
    started: Arc<(Mutex<bool>, Condvar)>,
    surface: RuntimeSurfaceHandle,
    mut client: RuntimeSurfaceClientHandle,
    control: TuiSurfaceTaskControl,
    task_id: String,
) {
    let (started_guard, wake) = &*started;
    let mut started_guard = match started_guard.lock() {
        Ok(guard) => guard,
        Err(_) => {
            detach_child_surface(&surface, &client);
            control.clear_any_focused_child_surface();
            return;
        }
    };
    while !*started_guard && !stop.load(Ordering::Acquire) {
        let result = wake.wait_timeout(started_guard, Duration::from_millis(100));
        started_guard = match result {
            Ok((guard, _)) => guard,
            Err(_) => {
                detach_child_surface(&surface, &client);
                control.clear_any_focused_child_surface();
                return;
            }
        };
    }
    drop(started_guard);
    if stop.load(Ordering::Acquire) {
        detach_child_surface(&surface, &client);
        control.clear_any_focused_child_surface();
        return;
    }

    for event in projection.hydrate_open_streams() {
        if event_tx.send(event).is_err() {
            detach_child_surface(&surface, &client);
            control.clear_any_focused_child_surface();
            return;
        }
    }
    let Some(mut operation_id) = operation_id else {
        detach_child_surface(&surface, &client);
        control.clear_any_focused_child_surface();
        return;
    };
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(item) = subscription.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        let terminal = match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                let mut terminal = batch.events.as_slice().iter().any(|envelope| {
                    matches!(
                        &envelope.event,
                        SurfaceEvent::Operation(
                            orca_runtime::surface::OperationPatch::Terminal { record }
                        ) if record.operation_id == operation_id
                    )
                });
                for envelope in batch.events.as_slice() {
                    if let SurfaceEvent::Interaction(
                        orca_runtime::surface::InteractionPatch::Requested { interaction },
                    ) = &envelope.event
                        && let Some(event) = control.register_surface_interaction(interaction)
                        && event_tx.send(event).is_err()
                    {
                        terminal = true;
                        break;
                    }
                }
                match projection.project_typed_batch(&batch) {
                    Ok(events) => {
                        for event in events {
                            if event_tx.send(event).is_err() {
                                terminal = true;
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(format!(
                            "child live transcript projection failed: {error:?}"
                        )));
                        terminal = true;
                    }
                }
                terminal
            }
            SurfaceSubscriptionItem::Gap { required } => {
                match recover_child_surface(
                    &surface,
                    &mut client,
                    &mut subscription,
                    &mut operation_id,
                    &mut *projection,
                    &task_id,
                    &event_tx,
                    &control,
                ) {
                    Ok(()) => false,
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(format!(
                            "child live transcript recovery failed after {:?}: {error}",
                            required.reason
                        )));
                        true
                    }
                }
            }
            SurfaceSubscriptionItem::Sealed { .. } => {
                let _ = event_tx.send(TuiEvent::Error(
                    "child live transcript ended before the conversation completed".to_string(),
                ));
                true
            }
        };
        if terminal {
            break;
        }
    }
    detach_child_surface(&surface, &client);
    control.clear_any_focused_child_surface();
}

fn child_interaction_capabilities() -> BTreeSet<SurfaceInteractionKind> {
    BTreeSet::from([
        SurfaceInteractionKind::ToolApproval,
        SurfaceInteractionKind::PermissionRequest,
        SurfaceInteractionKind::UserInput,
        SurfaceInteractionKind::McpElicitation,
    ])
}

fn register_requested_interactions(
    snapshot: &SurfaceSnapshot,
    control: &TuiSurfaceTaskControl,
) -> Vec<TuiEvent> {
    snapshot
        .interactions
        .iter()
        .filter(|interaction| {
            matches!(
                interaction.lifecycle,
                SurfaceInteractionLifecycle::Requested
            )
        })
        .filter_map(|interaction| control.register_surface_interaction(interaction))
        .collect()
}

fn recover_child_surface(
    surface: &RuntimeSurfaceHandle,
    client: &mut RuntimeSurfaceClientHandle,
    subscription: &mut orca_runtime::surface::SurfaceSubscriptionReceiver,
    operation_id: &mut SurfaceOperationId,
    projection: &mut TuiSurfaceProjection,
    task_id: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) -> Result<(), String> {
    detach_child_surface(surface, client);
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: orca_runtime::surface::SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: child_interaction_capabilities(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { reason } => return Err(format!("attachment denied: {reason:?}")),
        AttachResult::Unavailable { reason } => {
            return Err(format!("attachment unavailable: {reason:?}"));
        }
        AttachResult::ThreadClosed { .. } => return Err("child thread closed".to_string()),
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. } => {
            return Err("fresh attachment was not returned".to_string());
        }
    };
    let snapshot = attachment.baseline.snapshot;
    let Some(next_operation) = snapshot
        .foreground_operation
        .as_ref()
        .filter(|operation| operation.terminal.is_none())
        .map(|operation| operation.operation_id.clone())
    else {
        detach_child_surface(surface, &attachment.client);
        return Err("child no longer has a running operation".to_string());
    };
    let projection_state = SurfaceProjectionState::from_surface_snapshot(&snapshot);
    let messages = crate::surface_projection::history_messages_from_surface_snapshot(&snapshot);
    control.clear_focused_child_surface(operation_id);
    *operation_id = next_operation.clone();
    *client = attachment.client.clone();
    if let Err(error) =
        control.install_focused_child_surface(attachment.client.clone(), next_operation)
    {
        detach_child_surface(surface, &attachment.client);
        return Err(error.to_string());
    }
    *projection = TuiSurfaceProjection::from_surface_snapshot(&snapshot);
    send_event(
        event_tx,
        TuiEvent::ChildProjectionReset {
            task_id: task_id.to_string(),
            projection: Box::new(projection_state),
        },
    )?;
    send_event(
        event_tx,
        TuiEvent::HistoryLoaded {
            messages,
            plan: None,
            label: "Recovered child conversation context.".to_string(),
        },
    )?;
    for event in register_requested_interactions(&snapshot, control) {
        send_event(event_tx, event)?;
    }
    let Some(next_subscription) = surface.claim_subscription(&attachment.subscription) else {
        detach_child_surface(surface, &attachment.client);
        return Err("recovered child subscription unavailable".to_string());
    };
    *subscription = next_subscription;
    Ok(())
}

fn send_event(event_tx: &mpsc::Sender<TuiEvent>, event: TuiEvent) -> Result<(), String> {
    event_tx
        .send(event)
        .map_err(|_| "TUI event receiver closed".to_string())
}

fn detach_child_surface(surface: &RuntimeSurfaceHandle, client: &RuntimeSurfaceClientHandle) {
    let _ = surface.detach(
        client,
        orca_runtime::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
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

    let parent_attachment = *session_attachment;
    let child_attachment = AttachmentRouting::allocate_next(attachment_routing);
    let parent_event_tx = event_tx.clone();
    let child_event_tx = spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        child_attachment,
        Some(attachment_routing.clone()),
    );
    let (child_bridge, child_projection, child_messages, child_interactions) =
        match HostedChildEventBridge::prepare(
            &child_thread,
            task_id.clone(),
            child_event_tx.clone(),
            control,
        ) {
            Ok(batch) => batch,
            Err(error) => {
                reject(event_tx, &error);
                return;
            }
        };

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

    if let Err(error) = project_hosted_thread_attached(
        child_projection,
        child_messages,
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
        *thread = Some(parent_thread.clone());
        *event_tx = parent_event_tx.clone();
        *session_attachment = parent_attachment;
        *focus = None;
        reject(
            event_tx,
            &format!("failed to project child conversation: {error}"),
        );
        return;
    }
    *focus = Some(HostedChildFocus {
        parent_thread,
        parent_event_tx,
        parent_attachment,
        child_thread: child_thread.clone(),
        child_event_tx,
        child_attachment,
        event_bridge: child_bridge,
    });
    // The projection and focus marker are sent through the root producer in
    // order. Start the relay only after the marker is queued so hydration and
    // live child events cannot race ahead of the visible focus transition.
    announce_runtime_ready(&child_thread, event_tx, control);
    let _ = send_attached_event(
        root_event_tx,
        child_attachment,
        TuiEvent::ChildFocusChanged {
            task_id: Some(task_id.clone()),
        },
    );
    for interaction in child_interactions {
        let _ = send_attached_event(root_event_tx, child_attachment, interaction);
    }
    if let Some(focus) = focus.as_ref() {
        focus.event_bridge.start();
    }
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
    let Some(mut child_focus) = focus.take() else {
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
        // Restore the child attachment as the active route before returning;
        // otherwise the focus record and event routing would disagree.
        // Keeping the focus record lets a subsequent return retry without
        // accepting a raw thread id.
        AttachmentRouting::switch_attachment(
            attachment_routing,
            root_event_tx,
            child_focus.child_attachment,
            Some(child_focus.parent_attachment),
            false,
        );
        AttachmentRouting::cancel_deferred_parent_events(attachment_routing, root_event_tx);
        *thread = Some(child_focus.child_thread.clone());
        *event_tx = child_focus.child_event_tx.clone();
        *session_attachment = child_focus.child_attachment;
        *focus = Some(child_focus);
        return;
    }
    child_focus.event_bridge.stop();
    AttachmentRouting::release_deferred_parent_events(attachment_routing, root_event_tx);
    announce_runtime_ready(
        thread.as_ref().expect("parent thread after child return"),
        event_tx,
        control,
    );
    let _ = send_attached_event(
        root_event_tx,
        child_focus.parent_attachment,
        TuiEvent::ChildFocusChanged { task_id: None },
    );
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
