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
    HostedConversation, hosted_projection_batch_from_snapshot, project_hosted_thread_attached,
};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TuiEvent};
use crate::surface_projection::{SurfaceProjectionState, TuiSurfaceProjection};
use crate::types::SideParentStatus;

pub(crate) struct FocusedAgent {
    task_id: String,
    started_at_ms: Option<u64>,
}

impl FocusedAgent {
    pub(crate) fn from_parent(task_id: &str, parent: &SurfaceSnapshot) -> Self {
        let started_at_ms = parent
            .tasks
            .iter()
            .find(|row| row.task_id.as_str() == task_id)
            .and_then(|row| row.started_at)
            .and_then(|time| u64::try_from(time.get()).ok());
        Self {
            task_id: task_id.to_owned(),
            started_at_ms,
        }
    }

    pub(crate) fn projection(&self, child: &SurfaceSnapshot) -> TuiSurfaceProjection {
        TuiSurfaceProjection::for_conversation(child, self.started_at_ms)
    }
}

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

/// Passive typed-surface subscription for a focused child's conversation. The
/// bridge owns no operation control; it only projects committed child batches
/// into the currently attached TUI sender. It follows the child from one
/// operation to the next: the one running when the child was focused, if any,
/// then each the child starts. It ends when it is stopped, when the TUI stops
/// receiving, or when the child's thread closes.
pub(crate) struct HostedChildEventBridge {
    main_mcp_subscription: Option<orca_mcp::McpChangeSubscription>,
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
        agent: FocusedAgent,
        event_tx: mpsc::Sender<TuiEvent>,
        control: &TuiSurfaceTaskControl,
    ) -> Result<
        (
            Self,
            SurfaceProjectionState,
            HostedConversation,
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
        let (initial_projection, conversation) =
            hosted_projection_batch_from_snapshot(thread, &snapshot);
        let mut projection = agent.projection(&snapshot);
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
                    agent,
                );
            })
            .map_err(|error| {
                detach_child_surface(&surface, &client);
                format!("failed to start child live transcript bridge: {error}")
            })?;
        Ok((
            Self {
                main_mcp_subscription: None,
                stop,
                started,
                handle: Some(handle),
                surface,
                client,
                control: control.clone(),
            },
            initial_projection,
            conversation,
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

    fn follow_main_catalog(
        &mut self,
        parent: &RuntimeThreadHandle,
        sender: mpsc::Sender<TuiEvent>,
    ) {
        let registry = parent.mcp_registry();
        let order = Mutex::new(());
        let report = Arc::new(move |registry: &orca_mcp::McpRegistry| {
            let _order = order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = sender.send(TuiEvent::MainMcpCatalogWhileVisiting(
                crate::surface_projection::McpCatalogView::from_registry(registry),
            ));
        });
        self.main_mcp_subscription = Some(registry.subscribe(report.clone()));
        report(&registry);
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
    agent: FocusedAgent,
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
    // The child's operation being followed, if any. The view stays open
    // between operations: a child just created has none yet, and one that
    // finished a turn starts another when it is sent a follow-up.
    let mut followed = operation_id;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(item) = subscription.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        let end = match item {
            SurfaceSubscriptionItem::Batch { batch } => {
                // Followed before the batch is projected: the projection
                // shows a turn's start and end only for the operation it is
                // focused on, and the child's requests are registered only
                // for the operation installed for it.
                if followed.is_none()
                    && let Some(next) = operation_started_in(&batch)
                {
                    projection.focus_operation(next.clone());
                    // This fails only once the controller is shutting down;
                    // the view, and this bridge with it, goes with it.
                    let _ = control.install_focused_child_surface(client.clone(), next.clone());
                    followed = Some(next);
                }
                let mut end = false;
                for envelope in batch.events.as_slice() {
                    if let SurfaceEvent::Interaction(
                        orca_runtime::surface::InteractionPatch::Requested { interaction },
                    ) = &envelope.event
                        && let Some(event) = control.register_surface_interaction(interaction)
                        && event_tx.send(event).is_err()
                    {
                        end = true;
                        break;
                    }
                }
                match projection.project_typed_batch(&batch) {
                    Ok(events) => {
                        for mut event in events {
                            if let TuiEvent::TurnStarted { started_at_ms, .. } = &mut event {
                                *started_at_ms = agent.started_at_ms;
                            }
                            if event_tx.send(event).is_err() {
                                end = true;
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(format!(
                            "child live transcript projection failed: {error:?}"
                        )));
                        end = true;
                    }
                }
                if let Some(ended) =
                    followed.take_if(|operation_id| operation_ended_in(&batch, operation_id))
                {
                    control.clear_focused_child_surface(&ended);
                }
                end
            }
            SurfaceSubscriptionItem::Gap { required } => {
                match recover_child_surface(
                    &surface,
                    &mut client,
                    &mut subscription,
                    &mut followed,
                    &mut *projection,
                    &agent,
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
                // A child whose last operation ended closes its thread when
                // it is finished: that is the end of its conversation.
                if followed.is_some() {
                    let _ = event_tx.send(TuiEvent::Error(
                        "child live transcript ended before the conversation completed".to_string(),
                    ));
                }
                true
            }
        };
        if end {
            break;
        }
    }
    detach_child_surface(&surface, &client);
    control.clear_any_focused_child_surface();
}

/// The first operation `batch` requests or admits: the one the child starts.
fn operation_started_in(
    batch: &orca_runtime::surface::SurfaceCommitBatch,
) -> Option<SurfaceOperationId> {
    batch
        .events
        .as_slice()
        .iter()
        .find_map(|envelope| match &envelope.event {
            SurfaceEvent::Operation(orca_runtime::surface::OperationPatch::Requested {
                operation,
            }) if operation.terminal.is_none() => Some(operation.operation_id.clone()),
            SurfaceEvent::Operation(orca_runtime::surface::OperationPatch::Admitted {
                operation_id,
                ..
            }) => Some(operation_id.clone()),
            _ => None,
        })
}

fn operation_ended_in(
    batch: &orca_runtime::surface::SurfaceCommitBatch,
    operation_id: &SurfaceOperationId,
) -> bool {
    batch.events.as_slice().iter().any(|envelope| {
        matches!(
            &envelope.event,
            SurfaceEvent::Operation(orca_runtime::surface::OperationPatch::Terminal { record })
                if &record.operation_id == operation_id
        )
    })
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
    operation_id: &mut Option<SurfaceOperationId>,
    projection: &mut TuiSurfaceProjection,
    agent: &FocusedAgent,
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
    // The child may be between operations; the view then follows none until
    // the child starts its next one.
    let next_operation = snapshot
        .foreground_operation
        .as_ref()
        .filter(|operation| operation.terminal.is_none())
        .map(|operation| operation.operation_id.clone());
    let projection_state = SurfaceProjectionState::from_surface_snapshot(&snapshot);
    let messages = crate::surface_projection::history_messages_from_surface_snapshot(&snapshot);
    if let Some(previous) = operation_id.take() {
        control.clear_focused_child_surface(&previous);
    }
    *client = attachment.client.clone();
    if let Some(next_operation) = next_operation.as_ref()
        && let Err(error) =
            control.install_focused_child_surface(attachment.client.clone(), next_operation.clone())
    {
        detach_child_surface(surface, &attachment.client);
        return Err(error.to_string());
    }
    *operation_id = next_operation;
    *projection = agent.projection(&snapshot);
    send_event(
        event_tx,
        TuiEvent::ChildProjectionReset {
            task_id: agent.task_id.clone(),
            main_status: None,
            projection: Box::new(projection_state),
        },
    )?;
    send_event(
        event_tx,
        HostedConversation::from_snapshot(&snapshot, messages).into_event(None),
    )?;
    for event in projection.hydrate_open_streams() {
        send_event(event_tx, event)?;
    }
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

pub(crate) fn project_hosted_child_attached(
    task_id: String,
    parent: &SurfaceSnapshot,
    projection: SurfaceProjectionState,
    conversation: HostedConversation,
    attachment: SessionAttachmentId,
    root_event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    send_attached_event(
        root_event_tx,
        attachment,
        TuiEvent::ChildProjectionReset {
            task_id,
            main_status: Some(main_status_for_child(parent)),
            projection: Box::new(projection),
        },
    )
    .map_err(|error| error.to_string())?;
    send_attached_event(root_event_tx, attachment, conversation.into_event(None))
        .map_err(|error| error.to_string())?;
    send_attached_event(
        root_event_tx,
        attachment,
        TuiEvent::MainMcpCatalogWhileVisiting(
            crate::surface_projection::McpCatalogView::from_surface(&parent.mcp_catalog),
        ),
    )
    .map_err(|error| error.to_string())
}

fn detach_child_surface(surface: &RuntimeSurfaceHandle, client: &RuntimeSurfaceClientHandle) {
    let _ = surface.detach(
        client,
        orca_runtime::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
}

fn main_status_for_child(snapshot: &SurfaceSnapshot) -> SideParentStatus {
    let Some(operation) = snapshot
        .foreground_operation
        .as_ref()
        .filter(|operation| operation.terminal.is_none())
    else {
        return SideParentStatus::Idle;
    };
    let mut status = SideParentStatus::Running;
    for interaction in &snapshot.interactions {
        if interaction.fence.operation_id != operation.operation_id
            || interaction.lifecycle != SurfaceInteractionLifecycle::Requested
        {
            continue;
        }
        match interaction.kind {
            SurfaceInteractionKind::UserInput => return SideParentStatus::NeedsInput,
            SurfaceInteractionKind::ToolApproval
            | SurfaceInteractionKind::PermissionRequest
            | SurfaceInteractionKind::McpElicitation => status = SideParentStatus::NeedsApproval,
            SurfaceInteractionKind::BackgroundApproval => {}
        }
    }
    status
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
    let binding = match resolve_child_binding(&parent_snapshot, &task_id, expected_revision) {
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

    // The host lookup can race the child's terminal commit. Read the parent
    // surface again and require the task to be bound to the same thread
    // before changing what is on screen.
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
    let current_binding =
        match resolve_child_binding(&current_snapshot, &task_id, expected_revision) {
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
    let (mut child_bridge, child_projection, child_messages, child_interactions) =
        match HostedChildEventBridge::prepare(
            &child_thread,
            FocusedAgent::from_parent(&task_id, &current_snapshot),
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

    if let Err(error) = project_hosted_child_attached(
        task_id.clone(),
        &current_snapshot,
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
    child_bridge.follow_main_catalog(&parent_thread, child_event_tx.clone());
    *focus = Some(HostedChildFocus {
        parent_thread,
        parent_event_tx,
        parent_attachment,
        child_thread: child_thread.clone(),
        child_event_tx,
        child_attachment,
        event_bridge: child_bridge,
    });
    // Start the relay only after the child reset is queued so hydration and
    // live child events cannot race ahead of the visible focus transition.
    // The prompt queue is not rebound here: the controller does that when
    // it takes up the child, which is not while it runs another turn.
    for interaction in child_interactions {
        let _ = send_attached_event(root_event_tx, child_attachment, interaction);
    }
    if let Some(focus) = focus.as_ref() {
        focus.event_bridge.start();
    }
}

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
    let projected = snapshot_for_return(
        attachment_routing,
        root_event_tx,
        child_focus.parent_attachment,
        || {
            read_parent_snapshot(&child_focus.parent_thread)
                .map_err(|error| format!("failed to return to parent conversation: {error}"))
        },
    )
    .and_then(|parent_snapshot| {
        project_returned_main(
            &child_focus.parent_thread,
            &parent_snapshot,
            child_focus.parent_attachment,
            root_event_tx,
            control,
            attachment_routing,
        )
        .map_err(|error| format!("failed to project parent conversation: {error}"))
    });
    if let Err(error) = projected {
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
        reject(event_tx, &error);
        *focus = Some(child_focus);
        return;
    }
    *thread = Some(child_focus.parent_thread.clone());
    *event_tx = child_focus.parent_event_tx.clone();
    *session_attachment = child_focus.parent_attachment;
    child_focus.event_bridge.stop();
    AttachmentRouting::release_deferred_parent_events(attachment_routing, root_event_tx);
    let _ = send_attached_event(
        root_event_tx,
        child_focus.parent_attachment,
        TuiEvent::ChildFocusChanged { task_id: None },
    );
}

fn reject(event_tx: &mpsc::Sender<TuiEvent>, message: &str) {
    let _ = event_tx.send(TuiEvent::OperationRejected(message.to_string()));
}

pub(crate) fn snapshot_for_return(
    routing: &Arc<Mutex<AttachmentRouting>>,
    sender: &mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
    read: impl FnOnce() -> Result<SurfaceSnapshot, String>,
) -> Result<SurfaceSnapshot, String> {
    AttachmentRouting::switch_attachment_deferred(routing, sender, attachment, None);
    read()
}

pub(crate) fn project_returned_main(
    thread: &RuntimeThreadHandle,
    snapshot: &SurfaceSnapshot,
    attachment: SessionAttachmentId,
    sender: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    routing: &Arc<Mutex<AttachmentRouting>>,
) -> Result<(), String> {
    let (projection, conversation) = hosted_projection_batch_from_snapshot(thread, snapshot);
    project_hosted_thread_attached(projection, conversation, None, attachment, sender)?;
    if let Some(generation) = snapshot
        .foreground_operation
        .as_ref()
        .or_else(|| snapshot.operation_history.last())
        .and_then(|operation| operation.generations.last())
    {
        send_attached_event(
            sender,
            attachment,
            TuiEvent::RuntimeTurnRestored(generation.logical_turn_id.clone()),
        )
        .map_err(|error| error.to_string())?;
    }
    let mut streams =
        TuiSurfaceProjection::for_conversation(snapshot, main_started_at_ms(thread, snapshot));
    for event in streams.hydrate_conversation() {
        send_attached_event(sender, attachment, event).map_err(|error| error.to_string())?;
    }
    if let Ok(queue) = thread.prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::List) {
        send_attached_event(sender, attachment, TuiEvent::PromptQueueUpdated(queue))
            .map_err(|error| error.to_string())?;
    }
    let requests = register_requested_interactions(snapshot, control);
    routing
        .lock()
        .map_err(|error| error.to_string())?
        .snapshot_projected(attachment, snapshot, &requests);
    for event in requests {
        send_attached_event(sender, attachment, event).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn main_started_at_ms(thread: &RuntimeThreadHandle, snapshot: &SurfaceSnapshot) -> Option<u64> {
    let operation = snapshot
        .foreground_operation
        .as_ref()
        .filter(|op| op.terminal.is_none())?;
    let generation_start = |generation: &orca_runtime::surface::GenerationRecord| {
        let task_id = operation
            .agent_loop_turns
            .iter()
            .filter(|turn| turn.fence == generation.fence)
            .find_map(|turn| turn.admitted_main_task_id.as_ref())?;
        let start = snapshot
            .tasks
            .iter()
            .find(|row| {
                &row.task_id == task_id
                    && row.task_type == orca_runtime::surface::SurfaceTaskType::MainSession
            })
            .and_then(|row| row.started_at)
            .map(|time| time.get())
            .or_else(|| {
                // Remove the cache fallback once admission publishes the main task's start on the surface.
                thread
                    .task_registry()
                    .get_cached(task_id.as_str())
                    .filter(|row| row.task_type == orca_core::task_types::TaskType::MainSession)
                    .and_then(|row| row.started_at_ms)
            })?;
        u64::try_from(start).ok()
    };
    let (first, later) = operation.generations.split_first()?;
    generation_start(first).or_else(|| later.iter().rev().find_map(generation_start))
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

fn resolve_child_binding(
    snapshot: &SurfaceSnapshot,
    task_id: &str,
    expected_revision: u64,
) -> Result<ChildBinding, String> {
    child_binding(snapshot, task_id, expected_revision)
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
    // A child that is writing moves its task's revision several times a
    // second, so the revision seen at the click is behind by the time this
    // runs. The task is the same task: its identity and the thread it is
    // bound to are what must hold, and `focus_child` reads the binding twice
    // to see that the thread did not change. Only a snapshot from before the
    // click is stale.
    if task.revision.get() < expected_revision {
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
    if subagent.status != orca_runtime::surface::SurfaceSubagentStatus::Running {
        return Err("the selected child is no longer running".to_string());
    }
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

/// A view with a live child on screen, for tests of what a message does
/// there. The child is a thread of a runtime host, as the bridge reads
/// nothing but a thread's surface.
#[cfg(test)]
pub(crate) mod live_child_for_test {
    use crate::hosted_view::HostedView;
    use crate::operation_controller::TuiSurfaceTaskControl;
    use crate::protocol::{SessionAttachmentId, TuiEvent};

    pub(crate) struct ViewOnChild {
        pub(crate) view: HostedView,
        pub(crate) control: TuiSurfaceTaskControl,
        // Kept so the view's senders stay connected.
        _events: crossbeam_channel::Receiver<TuiEvent>,
        host: orca_runtime::runtime_host::RuntimeHost,
        parent: orca_runtime::runtime_host::RuntimeThreadHandle,
        child: orca_runtime::runtime_host::RuntimeThreadHandle,
        _home: crate::test_support::OrcaHomeGuard,
    }

    impl ViewOnChild {
        /// The view is bound to `control` and shows the child.
        pub(crate) fn start() -> Self {
            Self::configured(|_| {})
        }

        fn configured(configure: impl FnOnce(&mut orca_core::config::RunConfig)) -> Self {
            let home = crate::test_support::isolate_orca_home();
            let mut config = crate::test_support::test_run_config();
            config.cwd = Some(home.path().to_path_buf());
            config.history_mode = orca_core::config::HistoryMode::Record;
            let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
            host.handle().keep_subagents_in_process();
            let child_config = config.clone();
            configure(&mut config);
            let parent = host
                .start_thread(config.clone(), "parent")
                .expect("parent thread");
            let child = host
                .start_thread(child_config, "child")
                .expect("child thread");
            let control = TuiSurfaceTaskControl::isolated_for_test();
            let (event_tx, events) = crossbeam_channel::unbounded();
            let main_attachment = SessionAttachmentId::new(1);
            let child_attachment = SessionAttachmentId::new(2);
            let view = HostedView::new(
                host.handle(),
                event_tx.clone(),
                std::sync::Arc::new(std::sync::Mutex::new(
                    crate::attachment_routing::AttachmentRouting::new(main_attachment),
                )),
                event_tx.clone(),
                main_attachment,
            );
            let (event_bridge, _projection, _messages, _interactions) =
                super::HostedChildEventBridge::prepare(
                    &child,
                    super::FocusedAgent {
                        task_id: "child".to_string(),
                        started_at_ms: None,
                    },
                    event_tx.clone(),
                    &control,
                )
                .expect("attach the child's view");
            view.show_child_for_test(super::HostedChildFocus {
                parent_thread: parent.clone(),
                parent_event_tx: event_tx.clone(),
                parent_attachment: main_attachment,
                child_thread: child.clone(),
                child_event_tx: event_tx,
                child_attachment,
                event_bridge,
            });
            control.bind_view(view.clone());
            Self {
                view,
                control,
                _events: events,
                host,
                parent,
                child,
                _home: home,
            }
        }

        pub(crate) fn shutdown(self) {
            self.control.unbind_view();
            drop(self.view.close());
            self.child.shutdown().expect("child thread shutdown");
            self.parent.shutdown().expect("parent thread shutdown");
            self.host.shutdown().expect("host shutdown");
        }

        fn adopt(&self) {
            let (mut sender, _events) = crossbeam_channel::unbounded();
            let mut thread = Some(self.parent.clone());
            let mut attachment = SessionAttachmentId::new(1);
            assert_eq!(
                self.view
                    .adopt(&mut thread, &mut sender, &mut attachment, false),
                crate::hosted_view::Adoption::Child
            );
            let thread = thread.unwrap();
            assert_eq!(thread.thread_id(), self.child.thread_id());
            crate::hosted_session::announce_runtime_ready(&thread, &sender, &self.control);
        }

        fn enqueue(&self, prompt: impl Into<String>) -> crossbeam_channel::Receiver<TuiEvent> {
            let (sender, events) = crossbeam_channel::unbounded();
            crate::hosted_session::announce_runtime_ready(&self.parent, &sender, &self.control);
            self.parent
                .prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::Add {
                    input: orca_runtime::prompt_queue::PromptQueueInput::text(prompt),
                })
                .unwrap();
            events
        }

        fn wait_for<T>(&self, mut find: impl FnMut(super::SurfaceSnapshot) -> Option<T>) -> T {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(result) = find(super::read_parent_snapshot(&self.parent).unwrap()) {
                    return result;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected change on main's surface; registry: {:?}",
                    self.parent
                        .task_registry()
                        .list()
                        .iter()
                        .map(|task| (&task.description, &task.status, &task.error))
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    fn self_started_main(shutdown: bool) {
        use orca_runtime::surface::{CancelReason, OperationTerminal};
        let fixture = ViewOnChild::start();
        let events = fixture.enqueue("mock_stream_delay_ms 30000");
        let running = fixture.wait_for(|snapshot| {
            (!snapshot.assistant_streams.is_empty())
                .then_some(snapshot.foreground_operation)?
                .filter(|op| op.terminal.is_none())
        });
        fixture.adopt();
        assert!(!fixture.child.is_busy());
        if shutdown {
            fixture.control.shutdown();
        } else {
            let (send, actions) = crossbeam_channel::unbounded();
            let mut state = crate::types::AppState::new(
                send.clone(),
                "test".into(),
                "mock".into(),
                "/tmp".into(),
            );
            state.update(TuiEvent::ChildFocusChanged {
                task_id: Some("child".into()),
            });
            let main = SessionAttachmentId::new(1);
            let routing = std::sync::Arc::new(std::sync::Mutex::new(
                crate::attachment_routing::AttachmentRouting::new(main),
            ));
            let (root, display) = crossbeam_channel::unbounded();
            crate::attachment_routing::AttachmentRouting::switch_attachment(
                &routing,
                &root,
                SessionAttachmentId::new(2),
                Some(main),
                false,
            );
            while state.main_behind_child != Some(crate::types::SideParentStatus::Running) {
                let event = events
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                routing.lock().unwrap().route(&root, main, event);
                for event in display.try_iter() {
                    if let TuiEvent::SideParentStatusChanged(_) = event {
                        state.update(event);
                    }
                }
            }
            assert!(
                crate::test_support::frame_string(&mut state, 120, 40).contains("Main is running")
            );
            crate::global_actions::handle_global_shortcut(
                crate::shortcuts::GlobalShortcut::Cancel,
                &mut state,
                &send,
                || Ok(()),
            )
            .unwrap();
            assert!(matches!(
                actions.try_recv(),
                Ok(crate::protocol::UserAction::Interrupt)
            ));
            assert!(state.last_ctrl_c.is_none());
            assert!(fixture.control.interrupt_current().unwrap());
        }
        let terminal_for_turn = |snapshot: super::SurfaceSnapshot| {
            snapshot
                .operation_history
                .into_iter()
                .chain(snapshot.foreground_operation)
                .find(|op| op.operation_id == running.operation_id)?
                .terminal
        };
        let terminal = if shutdown {
            terminal_for_turn(super::read_parent_snapshot(&fixture.parent).unwrap())
                .expect("shutdown returns after the queued turn ends")
        } else {
            fixture.wait_for(terminal_for_turn)
        };
        assert_eq!(
            terminal.terminal,
            OperationTerminal::Cancelled {
                reason: CancelReason::User
            }
        );
        assert!(fixture.parent.is_available());
        fixture.shutdown();
    }

    #[test]
    fn ctrl_c_in_an_agents_view_interrupts_a_running_main() {
        self_started_main(false);
    }

    #[test]
    fn shutdown_ends_a_turn_main_started_by_itself() {
        self_started_main(true);
    }

    #[test]
    fn a_transcript_read_from_an_agents_view_asks_mains_thread() {
        use crate::protocol::{TaskTranscriptRequest, TaskTranscriptResult};
        let fixture = ViewOnChild::configured(|config| {
            config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto
        });
        let _events = fixture.enqueue("subagent async say hello");
        let request = fixture.wait_for(|snapshot| {
            let task = snapshot.tasks.iter().find(|task| {
                task.task_type == orca_runtime::surface::SurfaceTaskType::Subagent
                    && task.status == orca_runtime::surface::SurfaceTaskStatus::Completed
            })?;
            Some(TaskTranscriptRequest {
                task_id: task.task_id.as_str().to_string(),
                expected_revision: task.revision.get(),
            })
        });
        assert!(
            matches!(crate::surface_client::read_task_transcript(&fixture.parent.typed_surface(), &request), TaskTranscriptResult::Found(record) if !record.items.is_empty())
        );
        fixture.adopt();
        let result = fixture.control.read_task_transcript(&request);
        fixture.shutdown();
        assert!(
            matches!(&result, Some(TaskTranscriptResult::Found(record)) if record.task_id.as_str() == request.task_id && !record.items.is_empty()),
            "{result:?}"
        );
    }

    #[test]
    fn main_lookup_stays_on_main_until_the_returned_thread_is_announced() {
        let fixture = ViewOnChild::start();
        fixture.adopt();
        fixture
            .view
            .handle(super::HostedChildAction::Return, &fixture.control);
        let mut thread = Some(fixture.child.clone());
        let (mut sender, _events) = crossbeam_channel::unbounded();
        let mut attachment = SessionAttachmentId::new(2);
        assert_eq!(
            fixture
                .view
                .adopt(&mut thread, &mut sender, &mut attachment, false),
            crate::hosted_view::Adoption::Main
        );
        assert_eq!(
            fixture.control.runtime_thread().unwrap().thread_id(),
            fixture.child.thread_id()
        );
        let owner = fixture.control.main_runtime().unwrap();
        assert_eq!(owner.thread_id(), fixture.parent.thread_id());
        fixture.shutdown();
    }

    #[test]
    fn a_stop_from_an_agents_view_reaches_mains_task() {
        use orca_runtime::surface::{SurfaceTaskStatus, SurfaceTaskType};
        let fixture = ViewOnChild::configured(|config| {
            config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto
        });
        let _events = fixture.enqueue("subagent async mock_stream_delay_ms 30000");
        let task = fixture.wait_for(|snapshot| {
            snapshot.tasks.into_iter().find(|task| {
                task.task_type == SurfaceTaskType::Subagent
                    && task.status == SurfaceTaskStatus::Running
            })
        });
        fixture.adopt();
        let (sender, _events) = crossbeam_channel::unbounded();
        let result = fixture
            .control
            .stop_background_task(task.task_id.as_str(), &sender);
        assert!(result.unwrap().is_some());
        fixture.wait_for(|snapshot| {
            snapshot.tasks.into_iter().find(|row| {
                row.task_id == task.task_id
                    && matches!(
                        row.status,
                        SurfaceTaskStatus::Stopped | SurfaceTaskStatus::Cancelled
                    )
            })
        });
        fixture.shutdown();
    }

    #[test]
    fn mcp_from_an_agents_view_lists_mains_servers() {
        let fixture = ViewOnChild::configured(|config| {
            config
                .mcp_servers
                .push(orca_core::mcp_types::McpServerConfig {
                    name: "archive".into(),
                    command: Some("never-launched".into()),
                    disabled: true,
                    ..Default::default()
                })
        });
        fixture.adopt();
        let Some(crate::prestart_mcp::McpServers::Thread(thread)) = fixture.control.mcp_servers()
        else {
            panic!("no thread")
        };
        let names: Vec<_> = super::read_parent_snapshot(&thread)
            .unwrap()
            .mcp_catalog
            .servers
            .iter()
            .map(|server| server.name.as_str().to_owned())
            .collect();
        fixture.shutdown();
        assert_eq!(names, ["archive"]);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use orca_core::conversation::ConversationTarget;
    use orca_core::cost_types::UsageTotals;
    use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};

    use orca_runtime::surface::{
        CanonicalPath, CompactionState, ContextRevision, CursorSourceRevision, DisplayText,
        DurableRevision, McpCatalogRevision, NonEmptyText, PinnedContextRevision, PlanRevision,
        PolicyEpoch, ProviderReplayHealth, SequenceNumber, SessionHealthRevision,
        SessionMetadataRevision, SettingsRevision, Sha256Digest, SubagentRevision,
        SurfaceApprovalMode, SurfaceCommitId, SurfaceContextSnapshot, SurfaceCursor,
        SurfaceGenerationId, SurfaceIncarnation, SurfaceMcpCatalogSnapshot,
        SurfaceNetworkPermissions, SurfaceOperationFence, SurfaceOperationId,
        SurfacePermissionRuleSet, SurfacePinnedContextSnapshot, SurfacePlanSnapshot,
        SurfaceReasoningEffort, SurfaceRuntimeSettings, SurfaceSessionHealth,
        SurfaceSettingsSnapshot, SurfaceSnapshot, SurfaceSubagent, SurfaceSubagentId,
        SurfaceSubagentOwner, SurfaceSubagentSource, SurfaceSubagentStatus, SurfaceTask,
        SurfaceTaskAttemptId, SurfaceTaskId, SurfaceTaskStatus, SurfaceTaskType, SurfaceThreadId,
        SurfaceThreadSnapshot, SurfaceTurnId, SurfaceUsageSnapshot, TaskRevision, ThreadOwnerEpoch,
        ThreadPersistence, UnixMillis, UsageRevision, UsageTotals as SurfaceUsageTotals,
    };

    use super::{ChildBinding, project_hosted_child_attached};
    use crate::attachment_routing::accept_attached_tui_event;
    use crate::protocol::SessionAttachmentId;
    use crate::surface_projection::SurfaceProjectionState;
    use crate::types::AppState;

    fn task(id: &str, task_type: TaskType) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            parent_task_id: None,
            task_type,
            status: TaskStatus::Running,
            is_backgrounded: false,
            lifetime: orca_core::task_types::TaskLifetime::Task,
            description: id.to_string(),
            created_at_ms: 1,
            sequence: 0,
            started_at_ms: Some(1),
            completed_at_ms: None,
            command: None,
            agent_type: None,
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
            publication_revision: Some(1),
        }
    }

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        bytes
    }

    /// A parent snapshot holding one running subagent task, `child`, at
    /// `revision`, bound to the hosted thread `child_thread_seed`.
    pub(crate) fn snapshot_with_child(revision: u64, child_thread_seed: u8) -> SurfaceSnapshot {
        let path = CanonicalPath::try_new(std::env::temp_dir().join("orca-tui-child-binding"))
            .expect("canonical test path");
        let task_id = SurfaceTaskId::try_new("child").unwrap();
        let subagent_id = SurfaceSubagentId::try_new("agent-child").unwrap();
        let thread_id = SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap();
        SurfaceSnapshot {
            cursor: SurfaceCursor {
                thread_id: thread_id.clone(),
                incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(2)).unwrap(),
                next_seq: SequenceNumber::new(0),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(1).unwrap(),
                },
            },
            thread: SurfaceThreadSnapshot {
                thread_id,
                owner_epoch: ThreadOwnerEpoch::new(1),
                persistence: ThreadPersistence::RecordedCatalogued,
                title: DisplayText::new("Parent"),
                metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
                created_at: UnixMillis::new(1),
                updated_at: UnixMillis::new(1),
                cwd: path.clone(),
                workspace_roots: vec![path.clone()],
                closed: false,
            },
            foreground_operation: None,
            queued_operations: Vec::new(),
            background_operations: Vec::new(),
            operation_history: Vec::new(),
            items: Vec::new(),
            assistant_streams: Vec::new(),
            tools: Vec::new(),
            plan: SurfacePlanSnapshot {
                revision: PlanRevision::try_new(1).unwrap(),
                explanation: None,
                items: Vec::new(),
                causative_generation: None,
            },
            usage: SurfaceUsageSnapshot {
                revision: UsageRevision::try_new(1).unwrap(),
                thread_total: SurfaceUsageTotals::default(),
                active_operation: None,
                goal: None,
                workflow: Vec::new(),
            },
            context: SurfaceContextSnapshot {
                revision: ContextRevision::try_new(1).unwrap(),
                window_id: orca_runtime::surface::ContextWindowId::new(),
                used_tokens: 0,
                limit_tokens: 128_000,
                compaction: CompactionState::Idle,
                fragments: Vec::new(),
                provider_replay: ProviderReplayHealth::None,
            },
            interactions: Vec::new(),
            tasks: vec![SurfaceTask {
                task_id: task_id.clone(),
                revision: TaskRevision::try_new(revision).unwrap(),
                task_type: SurfaceTaskType::Subagent,
                status: SurfaceTaskStatus::Running,
                backgrounded: false,
                description: DisplayText::new("child"),
                created_at: UnixMillis::new(1),
                started_at: Some(UnixMillis::new(1)),
                completed_at: None,
                parent_operation: None,
                parent_task_id: None,
                background_fence: None,
                workflow_run_id: None,
                subagent_id: Some(subagent_id.clone()),
                pending_interaction_id: None,
                usage: None,
                result: None,
                error: None,
                retry_count: 0,
                output_truncated: false,
            }],
            workflows: Vec::new(),
            subagents: vec![SurfaceSubagent {
                subagent_id,
                task_id,
                revision: SubagentRevision::try_new(1).unwrap(),
                description: DisplayText::new("child"),
                child_thread_id: Some(
                    SurfaceThreadId::try_from_bytes(uuid_v7_bytes(child_thread_seed)).unwrap(),
                ),
                batch_id: NonEmptyText::try_new("batch-child").unwrap(),
                batch_size: 1,
                status: SurfaceSubagentStatus::Running,
                activity: None,
                subagent_activity_history: Vec::new(),
                turn: Some(1),
                usage: None,
                output: None,
                error: None,
                continuation: None,
                owner: SurfaceSubagentOwner::Generation {
                    fence: SurfaceOperationFence {
                        thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
                        thread_owner_epoch: ThreadOwnerEpoch::new(1),
                        operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(3)).unwrap(),
                        generation_id: SurfaceGenerationId::new(3),
                    },
                },
                source: SurfaceSubagentSource::new(
                    SurfaceTaskAttemptId::try_new("attempt-child").unwrap(),
                    SurfaceTurnId::new(),
                    1,
                    UnixMillis::new(1),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
                    Sha256Digest::new([4; 32]),
                ),
            }],
            goal: None,
            settings: SurfaceSettingsSnapshot {
                host_revision: SettingsRevision::try_new(1).unwrap(),
                thread_revision: SettingsRevision::try_new(1).unwrap(),
                effective: SurfaceRuntimeSettings {
                    model: NonEmptyText::try_new("deepseek-v4").unwrap(),
                    reasoning_effort: SurfaceReasoningEffort::High,
                    approval_mode: SurfaceApprovalMode::AutoEdit,
                    cwd: path.clone(),
                    workspace_roots: vec![path],
                    active_permission_profile: None,
                    permission_rules: SurfacePermissionRuleSet {
                        ordered_rules: Vec::new(),
                        digest: Sha256Digest::new([1; 32]),
                    },
                    additional_working_directories: Vec::new(),
                    network_permissions: SurfaceNetworkPermissions {
                        enabled: Some(true),
                        domains: Vec::new(),
                    },
                    policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                },
                pending: None,
                frozen_generation_revision: None,
            },
            mcp_catalog: SurfaceMcpCatalogSnapshot {
                revision: McpCatalogRevision::try_new(1).unwrap(),
                servers: Vec::new(),
                tools: Vec::new(),
                prompts: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                diagnostics: Vec::new(),
            },
            pinned_context: SurfacePinnedContextSnapshot {
                revision: PinnedContextRevision::try_new(1).unwrap(),
                entries: Vec::new(),
            },
            session_health: SurfaceSessionHealth {
                revision: SessionHealthRevision::try_new(1).unwrap(),
                accepting_admission: true,
                issues: Vec::new(),
                closing: false,
                closed: false,
            },
        }
    }

    #[test]
    fn a_child_whose_revision_moved_on_is_still_the_one_selected() {
        let snapshot = snapshot_with_child(7, 9);
        let binding =
            super::child_binding(&snapshot, "child", 3).expect("a newer revision is the same task");
        assert_eq!(
            binding.child_thread_id,
            uuid::Uuid::from_bytes(uuid_v7_bytes(9)).to_string()
        );
    }

    #[test]
    fn a_snapshot_older_than_what_was_seen_is_refused() {
        let snapshot = snapshot_with_child(2, 9);
        let error =
            super::child_binding(&snapshot, "child", 3).expect_err("older than what was seen");
        assert_eq!(
            error,
            "the selected child task is stale (expected revision 3)"
        );
    }

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

    #[test]
    fn hosted_child_projection_sets_target_before_merging_child_tasks() {
        let (action_tx, _action_rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let parent_task = task("parent-task", TaskType::Workflow);
        let child_task = task("child-task", TaskType::Subagent);
        state.apply_workflow_tasks_for_test(vec![parent_task.clone()]);

        let attachment = SessionAttachmentId::new(2);
        state.active_session_attachment = Some(attachment);
        let projection = SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(2),
            session_id: Some("child-session".to_string()),
            title: "Child".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: vec![child_task.clone()],
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
            mcp_catalog: Default::default(),
        };
        let (root_tx, root_rx) = crossbeam_channel::unbounded();

        project_hosted_child_attached(
            child_task.id.clone(),
            &snapshot_with_child(1, 5),
            projection,
            super::HostedConversation {
                messages: Vec::new(),
                plan: None,
            },
            attachment,
            &root_tx,
        )
        .expect("project child");
        for index in 0..2 {
            let event = root_rx.recv().expect("hosted child event");
            if let Some(event) =
                accept_attached_tui_event(&mut state, event).expect("active attachment")
            {
                if index == 1 {
                    assert!(matches!(
                        event,
                        super::TuiEvent::ConversationReprojected { .. }
                    ));
                }
                state.update(event);
            }
        }

        assert_eq!(
            state.conversation_target(),
            &ConversationTarget::subagent(child_task.id.clone())
        );
        assert_eq!(
            state
                .background_workflow_tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec![parent_task.id.as_str()]
        );
        assert!(
            state
                .workflow_tasks()
                .iter()
                .any(|task| task.id == parent_task.id)
        );
        assert!(
            state
                .workflow_tasks()
                .iter()
                .any(|task| task.id == child_task.id)
        );
    }

    /// A thread of a runtime host standing in for a child kept in the
    /// process: the bridge reads nothing but the thread's surface, and a turn
    /// run here is an operation of that surface, as a delegated turn is.
    struct ChildThread {
        config: orca_core::config::RunConfig,
        thread: orca_runtime::runtime_host::RuntimeThreadHandle,
        host: orca_runtime::runtime_host::RuntimeHost,
        home: crate::test_support::OrcaHomeGuard,
    }

    impl ChildThread {
        fn start() -> Self {
            let home = crate::test_support::isolate_orca_home();
            let mut config = crate::test_support::test_run_config();
            config.cwd = Some(home.path().to_path_buf());
            config.history_mode = orca_core::config::HistoryMode::Record;
            let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
            let thread = host
                .start_thread(config.clone(), "child")
                .expect("child thread");
            Self {
                config,
                thread,
                host,
                home,
            }
        }

        /// Attaches a bridge the way a focus does and lets it read.
        fn open_view(
            &self,
            control: &crate::operation_controller::TuiSurfaceTaskControl,
        ) -> (
            super::HostedChildEventBridge,
            SurfaceProjectionState,
            crossbeam_channel::Receiver<crate::protocol::TuiEvent>,
        ) {
            let (event_tx, events) = crossbeam_channel::unbounded();
            let (bridge, projection, _messages, _interactions) =
                super::HostedChildEventBridge::prepare(
                    &self.thread,
                    super::FocusedAgent {
                        task_id: "child".to_string(),
                        started_at_ms: None,
                    },
                    event_tx,
                    control,
                )
                .expect("attach the child's view");
            bridge.start();
            (bridge, projection, events)
        }

        fn shutdown(self) {
            self.thread.shutdown().expect("child thread shutdown");
            self.host.shutdown().expect("host shutdown");
            drop(self.home);
        }
    }

    /// Runs one turn on `thread`'s surface through a client of its own: it is
    /// reserved and then admitted there, as a delegated turn is. Returns once
    /// the turn has ended.
    fn run_child_turn(
        thread: &orca_runtime::runtime_host::RuntimeThreadHandle,
        config: orca_core::config::RunConfig,
        prompt: &str,
        event_tx: &crossbeam_channel::Sender<crate::protocol::TuiEvent>,
    ) {
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let outcome = crate::surface_actions::TuiSurfaceActions::new(thread.typed_surface())
            .run_turn(
                orca_runtime::runtime_host::HostedTurnRequest::new(prompt),
                config,
                &control,
                event_tx,
            )
            .expect("the child's turn");
        assert!(
            matches!(
                &outcome,
                crate::hosted_runtime::TuiHostedOperationOutcome::Turn { status }
                    if status == "success"
            ),
            "the child's turn succeeds"
        );
    }

    pub(crate) fn with_streaming_main<T>(
        read: impl FnOnce(&super::RuntimeThreadHandle, SurfaceSnapshot) -> T,
    ) -> T {
        let main = ChildThread::start();
        let marker = main.home.path().join("release-streaming-main");
        let (sender, events) = crossbeam_channel::unbounded();
        let turn = {
            let thread = main.thread.clone();
            let config = main.config.clone();
            let prompt = format!("mock_stream_release_marker {}", marker.display());
            std::thread::spawn(move || run_child_turn(&thread, config, &prompt, &sender))
        };
        while !matches!(
            events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            crate::protocol::TuiEvent::MessageDelta(_)
        ) {}
        let snapshot = super::read_parent_snapshot(&main.thread).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            read(&main.thread, snapshot)
        }));
        std::fs::write(&marker, b"").unwrap();
        turn.join().unwrap();
        main.shutdown();
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    /// What the view was sent for one turn of the child.
    struct ViewedTurn {
        started: bool,
        reply: String,
    }

    /// Reads what the bridge sends up to the end of the child's next turn,
    /// keeping every event in `seen`. A turn that does not reach the view
    /// within ten seconds, or a bridge that ends first, fails the test.
    fn next_viewed_turn(
        events: &crossbeam_channel::Receiver<crate::protocol::TuiEvent>,
        seen: &mut Vec<crate::protocol::TuiEvent>,
    ) -> ViewedTurn {
        use crate::protocol::TuiEvent;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut turn = ViewedTurn {
            started: false,
            reply: String::new(),
        };
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = match events.recv_timeout(remaining) {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("the child's turn did not reach its view; it was sent {seen:?}")
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    panic!("the child's view ended before its turn did; it was sent {seen:?}")
                }
            };
            let ended = match &event {
                TuiEvent::TurnStarted { .. } => {
                    turn.started = true;
                    None
                }
                TuiEvent::MessageDelta(text) => {
                    turn.reply.push_str(text);
                    None
                }
                // A completed response replaces what was streamed.
                TuiEvent::AssistantResponseCompleted(Some(text), _) => {
                    turn.reply = text.clone();
                    None
                }
                TuiEvent::SessionCompleted { status } => Some(status.clone()),
                _ => None,
            };
            seen.push(event);
            if let Some(status) = ended {
                assert_eq!(status, "success", "the child's turn ends as it did");
                return turn;
            }
        }
    }

    #[test]
    fn a_child_opened_before_its_turn_starts_is_followed_once_it_does() {
        let child = ChildThread::start();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let (mut bridge, projection, events) = child.open_view(&control);
        assert!(
            projection.foreground_operation_id.is_none(),
            "the child has no operation when its view opens"
        );

        let (turn_tx, _turn_events) = crossbeam_channel::unbounded();
        run_child_turn(
            &child.thread,
            child.config.clone(),
            "hello\nmock_history_echo",
            &turn_tx,
        );
        let mut seen = Vec::new();
        let turn = next_viewed_turn(&events, &mut seen);

        assert!(turn.started, "the view shows the turn starting: {seen:?}");
        assert!(
            turn.reply.contains("hello"),
            "the view shows the child's reply, got {:?}",
            turn.reply
        );
        bridge.stop();
        child.shutdown();
    }

    #[test]
    fn the_view_goes_on_to_the_childs_next_turn() {
        let child = ChildThread::start();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        // The first turn streams its first words and then waits for the
        // marker, so the view opens on it while it runs.
        let marker = child.home.path().join("release-the-first-turn");
        let (first_tx, first_events) = crossbeam_channel::unbounded();
        let first_turn = {
            let thread = child.thread.clone();
            let config = child.config.clone();
            let prompt = format!("mock_stream_release_marker {}", marker.display());
            std::thread::spawn(move || run_child_turn(&thread, config, &prompt, &first_tx))
        };
        loop {
            match first_events.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(crate::protocol::TuiEvent::MessageDelta(_)) => break,
                Ok(_) => {}
                Err(error) => panic!("the first turn did not start streaming: {error:?}"),
            }
        }
        let (mut bridge, projection, events) = child.open_view(&control);
        assert!(
            projection.foreground_operation_id.is_some(),
            "the view opens on the running first turn"
        );

        std::fs::write(&marker, b"").expect("release the first turn");
        first_turn.join().expect("the first turn's client");
        let mut seen = Vec::new();
        let first = next_viewed_turn(&events, &mut seen);
        let (second_tx, _second_events) = crossbeam_channel::unbounded();
        run_child_turn(
            &child.thread,
            child.config.clone(),
            "two\nmock_history_echo",
            &second_tx,
        );
        let second = next_viewed_turn(&events, &mut seen);

        assert!(first.started, "the view shows the first turn: {seen:?}");
        assert_eq!(
            first.reply,
            "Mock release-marker stream started.Mock release-marker stream completed."
        );
        assert!(
            second.started,
            "the view shows the next turn starting: {seen:?}"
        );
        assert!(
            second.reply.contains("two"),
            "the view shows the next turn's reply after the first's, got {:?}",
            second.reply
        );

        // The child's thread closes after its last turn: the view ends there
        // on its own, and says nothing went wrong.
        let ChildThread {
            thread, host, home, ..
        } = child;
        thread.shutdown().expect("the child's thread closes");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Ok(event) => seen.push(event),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("the view did not end with the child's thread; it was sent {seen:?}")
                }
            }
        }
        bridge.stop();
        host.shutdown().expect("host shutdown");
        drop(home);
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, crate::protocol::TuiEvent::Error(_))),
            "the view reports no error: {seen:?}"
        );
    }
}
