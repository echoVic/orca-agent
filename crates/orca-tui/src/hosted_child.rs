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
    HistoryTarget, HostedConversation, hosted_projection_batch_from_snapshot,
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
            hosted_projection_batch_from_snapshot(thread, &snapshot, HistoryTarget::Conversation);
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
        self.main_mcp_subscription = None;
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
            interaction.lifecycle == SurfaceInteractionLifecycle::Requested
                && !crate::surface_client::snapshot_operation_is_terminal(
                    snapshot,
                    &interaction.fence.operation_id,
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
    let conversation = HostedConversation::from_snapshot(
        &snapshot,
        HistoryTarget::Conversation,
        &projection_state,
    );
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
    send_event(event_tx, conversation.into_event(None))?;
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
    if let Some(status) = control
        .pending_queue_interactions(parent_thread.thread_id())
        .iter()
        .find_map(crate::attachment_routing::side_parent_status_for_event)
    {
        let _ = send_attached_event(
            root_event_tx,
            child_attachment,
            TuiEvent::SideParentStatusChanged(status),
        );
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
    routing
        .lock()
        .map_err(|error| error.to_string())?
        .defer_parent(attachment);
    let snapshot = read()?;
    AttachmentRouting::switch_attachment_deferred(routing, sender, attachment, None);
    Ok(snapshot)
}

pub(crate) fn project_returned_main(
    thread: &RuntimeThreadHandle,
    snapshot: &SurfaceSnapshot,
    attachment: SessionAttachmentId,
    sender: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    routing: &Arc<Mutex<AttachmentRouting>>,
) -> Result<(), String> {
    project_returned_main_with_label(thread, snapshot, attachment, sender, control, routing, None)
}

pub(crate) fn project_returned_main_with_label(
    thread: &RuntimeThreadHandle,
    snapshot: &SurfaceSnapshot,
    attachment: SessionAttachmentId,
    sender: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    routing: &Arc<Mutex<AttachmentRouting>>,
    label: Option<&str>,
) -> Result<(), String> {
    let (projection, conversation) =
        hosted_projection_batch_from_snapshot(thread, snapshot, HistoryTarget::Conversation);
    let owner = snapshot
        .foreground_operation
        .as_ref()
        .filter(|owner| owner.terminal.is_none());
    let recovery = owner.and_then(|owner| {
        control
            .follow_recovery(&owner.operation_id)
            .map(|message| (&owner.operation_id, message))
    });
    let requests = register_requested_interactions(snapshot, control);
    let reader_live = owner
        .and_then(|owner| control.main_reader(&owner.operation_id))
        .is_some_and(|reader| reader.seed(&snapshot.cursor));
    let seed = routing
        .lock()
        .map_err(|error| error.to_string())?
        .snapshot_projected(
            attachment,
            snapshot,
            &requests,
            recovery.map(|(owner, _)| owner.clone()),
            reader_live,
        );
    send_attached_event(
        sender,
        attachment,
        TuiEvent::ReturnedMainProjection(Box::new(projection), seed),
    )
    .map_err(|error| error.to_string())?;
    send_attached_event(sender, attachment, conversation.into_event(label))
        .map_err(|error| error.to_string())?;
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
    if let Some((owner, message)) = recovery {
        send_attached_event(
            sender,
            attachment,
            TuiEvent::TerminalRecoveryNeeded(message.into()).at_operation_cursor(
                true,
                &snapshot.cursor,
                owner,
            ),
        )
        .map_err(|error| error.to_string())?;
    }
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
            Self::with_host(
                || orca_runtime::runtime_host::RuntimeHost::start().unwrap(),
                configure,
            )
        }

        fn with_host(
            start_host: impl FnOnce() -> orca_runtime::runtime_host::RuntimeHost,
            configure: impl FnOnce(&mut orca_core::config::RunConfig),
        ) -> Self {
            let home = crate::test_support::isolate_orca_home();
            let host = start_host();
            let mut config = crate::test_support::test_run_config();
            config.cwd = Some(home.path().to_path_buf());
            config.history_mode = orca_core::config::HistoryMode::Record;
            host.handle().keep_subagents_in_process();
            let child_config = config.clone();
            configure(&mut config);
            let parent = host
                .start_thread(config.clone(), "parent")
                .expect("parent thread");
            let child = host
                .handle()
                .start_side_thread(&parent, child_config, "child")
                .expect("child thread");
            assert_eq!(child.parent_thread_id(), Some(parent.thread_id()));
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

    #[test]
    fn queued_text_tool_text_and_terminal_follow_one_cursor() {
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("release-tool");
        let events = fixture.enqueue(format!(
            "mock_stream_tool_release_marker {}",
            serde_json::json!({"marker":marker,"toolPrompt":"task_list"})
        ));
        let mut observed = Vec::new();
        let mut last = 0;
        loop {
            let delivery = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let (cursor, event) = match delivery {
                TuiEvent::SurfaceDelivery(pair) => (Some(pair.0.next_seq.get()), pair.1),
                other => (None, other),
            };
            let kind = match event {
                TuiEvent::MessageDelta(ref text)
                    if text.contains("Mock slow tool stream started.") =>
                {
                    std::fs::write(&marker, "").unwrap();
                    "prefix"
                }
                TuiEvent::ToolRequested { .. } => "tool",
                TuiEvent::MessageDelta(ref text)
                    if text.contains("Mock completed after tool execution.") =>
                {
                    "tail"
                }
                TuiEvent::SessionCompleted { .. } => "terminal",
                _ => continue,
            };
            let cursor =
                cursor.unwrap_or_else(|| panic!("{kind} did not come from a committed batch"));
            assert!(cursor >= last);
            last = cursor;
            observed.push(kind);
            if kind == "terminal" {
                break;
            }
        }
        assert_eq!(observed, ["prefix", "tool", "tail", "terminal"]);
        fixture.shutdown();
    }

    #[test]
    fn settings_after_return_target_main_before_the_controller_adopts_it() {
        let fixture = ViewOnChild::start();
        fixture.adopt();
        fixture
            .control
            .bind_hosted_settings_config(std::sync::Arc::new(std::sync::Mutex::new(
                crate::test_support::test_run_config(),
            )));
        let marker = fixture._home.path().join("hold-settings-child");
        let operation = fixture
            .child
            .start_turn(
                orca_runtime::runtime_host::HostedTurnRequest::new(format!(
                    "mock_stream_release_marker {}",
                    marker.display()
                )),
                std::io::sink(),
            )
            .unwrap();
        fixture
            .view
            .handle(super::HostedChildAction::Return, &fixture.control);
        let encoded = crate::slash_command_actions::encode_confirmed_full_access_intent(None, None);
        let stamped = fixture.control.stamp_settings_action(encoded);
        assert!(fixture.child.is_busy());
        assert!(!fixture.control.try_apply_active_full_access(&stamped));
        let queued = crate::hosted_settings::QueuedSettings::decode(&stamped).unwrap();
        assert_eq!(
            queued.thread_id.as_deref(),
            Some(fixture.parent.thread_id())
        );
        assert_eq!(
            fixture.control.runtime_thread().unwrap().thread_id(),
            fixture.child.thread_id()
        );
        std::fs::write(marker, "").unwrap();
        operation
            .wait_timeout(std::time::Duration::from_secs(5))
            .expect("child finished");
        fixture.shutdown();
    }

    #[derive(Clone, Copy)]
    enum RequestTiming {
        BeforeVisit,
        BeforeAdoption,
        AfterAdoption,
        Cancelled,
        LegacyTerminal,
    }

    fn queued_approval(timing: RequestTiming) {
        use crate::attachment_routing::AttachmentRouting;
        let fixture = ViewOnChild::start();
        let parent = SessionAttachmentId::new(1);
        let routing = std::sync::Arc::new(std::sync::Mutex::new(AttachmentRouting::new(parent)));
        let (root, displayed) = crossbeam_channel::unbounded();
        let visit = || {
            AttachmentRouting::switch_attachment(
                &routing,
                &root,
                parent.next(),
                Some(parent),
                false,
            )
        };
        let marker = fixture._home.path().join("ask");
        let witness = fixture._home.path().join("witness.txt");
        std::fs::write(&witness, "before").unwrap();
        let events = fixture.enqueue(format!(
            "mock_stream_tool_release_marker {}",
            serde_json::json!({"marker":marker,"toolPrompt":"edit witness.txt :: before => after"})
        ));
        if !matches!(timing, RequestTiming::BeforeVisit) {
            visit();
        }
        if matches!(timing, RequestTiming::AfterAdoption) {
            fixture.adopt();
        }
        std::fs::write(marker, "").unwrap();
        let request = loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("request on main's relay");
            if matches!(&event, TuiEvent::SurfaceDelivery(delivery) if matches!(delivery.1, TuiEvent::ApprovalNeeded { .. }))
            {
                break event;
            }
        };
        routing
            .lock()
            .unwrap()
            .route(&root, parent, request.clone());
        let before = displayed.try_iter().collect::<Vec<_>>();
        if matches!(timing, RequestTiming::BeforeVisit) {
            assert!(before.iter().any(|event| matches!(event, TuiEvent::Attached(item) if item.attachment == Some(parent) && matches!(&item.event, TuiEvent::SurfaceDelivery(delivery) if matches!(delivery.1, TuiEvent::ApprovalNeeded { .. })))));
            visit();
            let (projection, conversation) = crate::hosted_session::read_hosted_projection_batch(
                &fixture.child,
                super::HistoryTarget::Conversation,
            )
            .unwrap();
            super::project_hosted_child_attached(
                "child".into(),
                &super::read_parent_snapshot(&fixture.parent).unwrap(),
                projection,
                conversation,
                parent.next(),
                &root,
            )
            .unwrap();
            let mut entering = fresh_state();
            for event in displayed.try_iter() {
                if let Some(event) =
                    crate::attachment_routing::accept_attached_tui_event(&mut entering, event)
                        .unwrap()
                {
                    entering.update(event);
                }
            }
            assert_eq!(
                entering.main_behind_child,
                Some(super::SideParentStatus::NeedsApproval)
            );
        } else {
            assert!(before.iter().any(|event| matches!(
                event,
                TuiEvent::SideParentStatusChanged(super::SideParentStatus::NeedsApproval)
            )));
            assert!(!before.iter().any(|event| matches!(event,TuiEvent::Attached(item) if matches!(item.event,TuiEvent::ApprovalNeeded{..}))));
        }
        if matches!(timing, RequestTiming::Cancelled) {
            assert!(fixture.control.interrupt_current().unwrap());
            fixture.wait_for(|snapshot| snapshot.operation_history.iter().any(|operation| matches!(&operation.terminal, Some(record) if matches!(record.terminal, orca_runtime::surface::OperationTerminal::Cancelled { .. }))).then_some(()));
        }
        let mut snapshot = super::snapshot_for_return(&routing, &root, parent, || {
            super::read_parent_snapshot(&fixture.parent)
        })
        .unwrap();
        if matches!(timing, RequestTiming::LegacyTerminal) {
            use orca_runtime::surface::{
                CancelReason, OperationPhase, OperationTerminal, OperationTerminalRecord,
                SurfaceFinalizeIntentId, UnixMillis,
            };
            let mut ended = snapshot.foreground_operation.take().unwrap();
            assert!(
                snapshot
                    .interactions
                    .iter()
                    .any(|request| request.fence.operation_id == ended.operation_id
                        && request.lifecycle
                            == orca_runtime::surface::SurfaceInteractionLifecycle::Requested)
            );
            ended.phase = OperationPhase::Terminal;
            ended.terminal = Some(OperationTerminalRecord {
                operation_id: ended.operation_id.clone(),
                finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(
                    *ended.operation_id.as_bytes(),
                )
                .unwrap(),
                terminal: OperationTerminal::Cancelled {
                    reason: CancelReason::User,
                },
                usage: Default::default(),
                source_diagnostic_digest: None,
                settlement_receipts: vec![],
                completion_proof: Default::default(),
                committed_at: UnixMillis::new(1),
            });
            snapshot.operation_history.push(ended);
        }
        super::project_returned_main(
            &fixture.parent,
            &snapshot,
            parent,
            &root,
            &fixture.control,
            &routing,
        )
        .unwrap();
        AttachmentRouting::release_deferred_parent_events(&routing, &root);
        let displayed_keys = || {
            displayed
                .try_iter()
                .filter_map(|event| match event {
                    TuiEvent::Attached(item) => match item.event {
                        TuiEvent::ApprovalNeeded { key, .. } => Some(key),
                        TuiEvent::SurfaceDelivery(delivery) => match delivery.1 {
                            TuiEvent::ApprovalNeeded { key, .. } => Some(key),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let keys = displayed_keys();
        routing
            .lock()
            .unwrap()
            .route(&root, parent, request.clone());
        let repeats = displayed_keys();
        let TuiEvent::SurfaceDelivery(delivery) = &request else {
            unreachable!()
        };
        let key = match &delivery.1 {
            TuiEvent::ApprovalNeeded { key, .. } => key,
            _ => unreachable!(),
        };
        if matches!(timing, RequestTiming::Cancelled) {
            assert!(
                keys.is_empty() && repeats.is_empty(),
                "cancelled request reopened"
            );
            assert_eq!(std::fs::read_to_string(witness).unwrap(), "before");
            fixture.shutdown();
            return;
        }
        assert!(
            fixture
                .control
                .respond_surface_interaction(
                    key,
                    &crate::protocol::TuiInteractionResponse::Approval(false)
                )
                .unwrap()
        );
        fixture.wait_for(|snapshot| {
            snapshot
                .operation_history
                .iter()
                .any(|operation| operation.terminal.is_some())
                .then_some(())
        });
        assert_eq!(std::fs::read_to_string(witness).unwrap(), "before");
        fixture.shutdown();
        if matches!(timing, RequestTiming::LegacyTerminal) {
            assert!(
                keys.is_empty() && repeats.is_empty(),
                "returned view offered an approval for a terminal operation: {keys:?}"
            );
            return;
        }
        assert_eq!(keys, [key.clone()], "restore, before any redelivery");
        assert!(repeats.is_empty(), "original request delivered again");
    }

    #[test]
    fn queued_approval_before_visit_is_restored() {
        queued_approval(RequestTiming::BeforeVisit);
    }
    #[test]
    fn queued_approval_before_adoption_stays_on_main() {
        queued_approval(RequestTiming::BeforeAdoption);
    }
    #[test]
    fn queued_approval_after_adoption_stays_on_main() {
        queued_approval(RequestTiming::AfterAdoption);
    }

    #[test]
    fn cancelled_queued_approval_is_not_restored() {
        queued_approval(RequestTiming::Cancelled);
    }

    #[test]
    fn return_ignores_requested_interaction_of_a_terminal_operation() {
        queued_approval(RequestTiming::LegacyTerminal);
    }

    fn fresh_state() -> crate::types::AppState {
        crate::types::AppState::new(
            crossbeam_channel::unbounded().0,
            "test".into(),
            "mock".into(),
            "/tmp".into(),
        )
    }

    fn returned_state(fixture: &ViewOnChild) -> crate::types::AppState {
        use crate::attachment_routing::{AttachmentRouting, accept_attached_tui_event};
        let parent = SessionAttachmentId::new(1);
        let routing = std::sync::Arc::new(std::sync::Mutex::new(AttachmentRouting::new(parent)));
        let (root, displayed) = crossbeam_channel::unbounded();
        AttachmentRouting::switch_attachment(&routing, &root, parent.next(), Some(parent), false);
        let snapshot = super::snapshot_for_return(&routing, &root, parent, || {
            super::read_parent_snapshot(&fixture.parent)
        })
        .unwrap();
        super::project_returned_main(
            &fixture.parent,
            &snapshot,
            parent,
            &root,
            &fixture.control,
            &routing,
        )
        .unwrap();
        AttachmentRouting::release_deferred_parent_events(&routing, &root);
        let mut state = fresh_state();
        for event in displayed.try_iter() {
            if let Some(event) = accept_attached_tui_event(&mut state, event).unwrap() {
                state.update(event);
            }
        }
        state
    }

    #[test]
    fn queued_tool_committed_during_a_visit_is_rebuilt_in_order() {
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("release-read");
        std::fs::write(fixture._home.path().join("witness.txt"), "before").unwrap();
        let events = fixture.enqueue(format!(
            "mock_stream_tool_release_marker {}",
            serde_json::json!({"marker":marker,"toolPrompt":"read witness.txt","retainText":true})
        ));
        std::fs::write(marker, "").unwrap();
        while !matches!(events.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), TuiEvent::SurfaceDelivery(item) if matches!(item.1, TuiEvent::SessionCompleted { .. }))
        {
        }
        let frame = crate::test_support::frame_string(&mut returned_state(&fixture), 120, 40);
        let (_, after) = frame
            .split_once("Mock slow tool stream started.")
            .expect(&frame);
        let (between, _) = after
            .split_once("Mock completed after tool execution.")
            .expect(&frame);
        assert!(
            between
                .lines()
                .any(|row| row.contains("✓ read") && row.contains("witness.txt")),
            "{frame}"
        );
        assert_eq!(frame.matches("✓ read").count(), 1, "{frame}");
        fixture.shutdown();
    }

    #[test]
    fn a_gap_on_a_running_follow_cancels_its_request_and_ends_the_row() {
        use orca_runtime::{
            history::SessionStore,
            prompt_queue::{PromptQueueAction, PromptQueueInput},
            runtime_host::RuntimeHost,
            surface::{self, InteractionPatch, OperationPatch, SurfaceEvent},
        };
        use std::{
            sync::{Arc, mpsc},
            time::{Duration, Instant},
        };
        let timeout = Duration::from_secs(10);
        let home = crate::test_support::isolate_orca_home();
        let witness = home.path().join("witness.txt");
        std::fs::write(&witness, "before").unwrap();
        let host = RuntimeHost::start_with_subscriber_limit_for_test(32).unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_owned());
        config.history_mode = orca_core::config::HistoryMode::Record;
        config.approval_mode = orca_core::approval_types::ApprovalMode::Suggest;
        let thread = host.start_thread(config, "gap during approval").unwrap();
        let (send, follows) = mpsc::channel();
        thread.set_prompt_queue_surface_observer(
            Arc::new(|_: &orca_core::event_schema::EventEnvelope| Ok(())),
            send,
        );
        thread
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text("edit witness.txt :: before => after"),
            })
            .unwrap();
        let follow = follows.recv_timeout(timeout).unwrap();
        let operation = follow.operation.clone();
        let deadline = Instant::now() + timeout;
        let request = loop {
            let snapshot = super::read_parent_snapshot(&thread).unwrap();
            if let Some(request) = snapshot
                .interactions
                .iter()
                .find(|request| request.fence.operation_id == operation)
            {
                break request.interaction_id.clone();
            }
            assert!(Instant::now() < deadline, "no committed approval");
            std::thread::sleep(Duration::from_millis(10));
        };
        let path = SessionStore::new()
            .load_session(thread.thread_id())
            .unwrap()
            .path;
        let baseline = follow.baseline.cursor.clone();
        overflow_delayed_reader(&thread, 32);
        let owner = thread.clone();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (sender, events) = crossbeam_channel::unbounded();
        let reader = std::thread::spawn(move || {
            crate::surface_client::follow_queue_operation(follow, &owner, &control, &sender)
        });
        let mut state = fresh_state();
        let mut saw_running = false;
        let mut terminals = Vec::new();
        let limit = Instant::now() + timeout;
        loop {
            let event = match events.recv_deadline(limit) {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("gapped reader did not end")
                }
            };
            if let TuiEvent::SessionCompleted { status } = &event {
                terminals.push(status.clone());
            }
            state.update(event);
            saw_running |= state.status == crate::types::AppStatus::Running;
        }
        reader.join().unwrap().unwrap();
        let frame = crate::test_support::frame_string(&mut state, 120, 40);
        assert!(saw_running);
        assert_eq!(state.status, crate::types::AppStatus::Idle, "{frame}");
        assert_eq!(
            frame
                .matches("typed TUI subscription gap requires SlowSubscriber")
                .count(),
            1,
            "{frame}"
        );
        assert_eq!(terminals, ["cancelled"]);
        let mut requests = Vec::new();
        let mut cancelled = None;
        let mut terminal = None;
        let ledger = surface::JsonlSurfaceCommitLedger::new(path, baseline);
        for batch in ledger.recover_batches().unwrap().committed {
            for envelope in batch.events.as_slice() {
                match &envelope.event {
                    SurfaceEvent::Interaction(InteractionPatch::Requested { interaction })
                        if interaction.fence.operation_id == operation =>
                    {
                        requests.push(interaction.interaction_id.clone())
                    }
                    SurfaceEvent::Interaction(InteractionPatch::Cancelled {
                        interaction_id,
                        reason:
                            surface::InteractionCancelReason::OperationCancelled {
                                reason: surface::CancelReason::User,
                            },
                        ..
                    }) if *interaction_id == request => {
                        cancelled = Some(batch.cursor_after.next_seq)
                    }
                    SurfaceEvent::Interaction(InteractionPatch::RouteChanged {
                        interaction_id,
                        ..
                    }) if *interaction_id == request => {
                        panic!("a gapped follow cannot transfer its request")
                    }
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation =>
                    {
                        assert!(matches!(
                            record.terminal,
                            surface::OperationTerminal::Cancelled {
                                reason: surface::CancelReason::User
                            }
                        ));
                        terminal = Some(batch.cursor_after.next_seq);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(requests, [request]);
        assert!(
            cancelled.expect("committed User cancellation") < terminal.expect("committed terminal")
        );
        assert_eq!(std::fs::read_to_string(witness).unwrap(), "before");
        host.shutdown().unwrap();
    }

    #[test]
    fn fix4_shutdown_releases_a_gapped_reader_before_its_worker_finishes() {
        use orca_runtime::{
            prompt_queue::{PromptQueueAction, PromptQueueInput},
            runtime_host::RuntimeHost,
        };
        let timeout = std::time::Duration::from_secs(10);
        let home = crate::test_support::isolate_orca_home();
        let marker = home.path().join("held-after-interrupt");
        let host = RuntimeHost::start_with_subscriber_limit_for_test(32).unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_owned());
        config.history_mode = orca_core::config::HistoryMode::Record;
        let thread = host.start_thread(config, "shutdown gap waiter").unwrap();
        let (send, follows) = std::sync::mpsc::channel();
        thread.set_prompt_queue_surface_observer(
            std::sync::Arc::new(|_: &orca_core::event_schema::EventEnvelope| Ok(())),
            send,
        );
        thread
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text(format!(
                    "mock_stream_usage_after_cancel_marker {}",
                    marker.display()
                )),
            })
            .unwrap();
        let follow = follows.recv_timeout(timeout).unwrap();
        let deadline = std::time::Instant::now() + timeout;
        while !super::read_parent_snapshot(&thread)
            .unwrap()
            .assistant_streams
            .iter()
            .any(|stream| {
                stream
                    .text
                    .as_str()
                    .contains("Mock usage-after-cancel stream started.")
            })
        {
            assert!(std::time::Instant::now() < deadline, "worker not streaming");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        overflow_delayed_reader(&thread, 32);
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let observer = control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let (finished, done) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            finished
                .send(crate::surface_client::follow_queue_operation(
                    follow, &thread, &observer, &sender,
                ))
                .unwrap()
        });
        while !matches!(events.recv_timeout(timeout).unwrap(), TuiEvent::Error(_)) {}
        control.shutdown();
        let before_release = done.recv_timeout(std::time::Duration::from_secs(1));
        std::fs::write(marker, "").unwrap();
        worker.join().unwrap();
        host.shutdown().unwrap();
        before_release
            .expect("gap waiter ignored shutdown")
            .unwrap();
    }

    #[test]
    fn fix4_refused_gap_interrupt_reports_one_error_and_ends_the_row() {
        refused_gap_interrupt(false);
    }

    #[test]
    fn fix5_refused_gap_delivers_one_owned_diagnostic_and_failed_close() {
        refused_gap_interrupt(true);
    }

    fn refused_gap_interrupt(routed: bool) {
        use orca_runtime::{
            history::SessionStore,
            prompt_queue::{PromptQueueAction, PromptQueueInput},
            runtime_host::RuntimeHost,
        };
        let timeout = std::time::Duration::from_secs(10);
        let home = crate::test_support::isolate_orca_home();
        let marker = home.path().join("release-refused-turn");
        let host = RuntimeHost::start_with_subscriber_limit_for_test(32).unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_owned());
        config.history_mode = orca_core::config::HistoryMode::Record;
        let thread = host.start_thread(config, "refused gap interrupt").unwrap();
        let (send, follows) = std::sync::mpsc::channel();
        thread.set_prompt_queue_surface_observer(
            std::sync::Arc::new(|_: &orca_core::event_schema::EventEnvelope| Ok(())),
            send,
        );
        thread
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text(format!(
                    "mock_stream_release_marker {}",
                    marker.display()
                )),
            })
            .unwrap();
        let follow = follows.recv_timeout(timeout).unwrap();
        let operation = follow.operation.clone();
        let path = SessionStore::new()
            .load_session(thread.thread_id())
            .unwrap()
            .path;
        let baseline = follow.baseline.cursor.clone();
        overflow_delayed_reader(&thread, 32);
        let held = path.with_extension("held");
        std::fs::rename(&path, &held).unwrap();
        std::fs::create_dir(&path).unwrap();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (display, events) = crossbeam_channel::unbounded();
        let main = SessionAttachmentId::new(1);
        if routed {
            control.bind_view(HostedView::new(
                host.handle(),
                display.clone(),
                std::sync::Arc::new(std::sync::Mutex::new(
                    crate::attachment_routing::AttachmentRouting::new(main),
                )),
                display.clone(),
                main,
            ));
        }
        let result =
            crate::surface_client::follow_queue_operation(follow, &thread, &control, &display);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(held, &path).unwrap();
        std::fs::write(marker, "").unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(terminal) = super::read_parent_snapshot(&thread)
                .unwrap()
                .operation_history
                .into_iter()
                .find(|entry| entry.operation_id == operation)
                .and_then(|entry| entry.terminal)
            {
                assert!(
                    matches!(
                        terminal.terminal,
                        orca_runtime::surface::OperationTerminal::Succeeded { .. }
                    ),
                    "refused interrupt must not stop worker: {:?}",
                    terminal.terminal
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        host.shutdown().unwrap();
        result.expect("the reader must not report a second error to its dispatcher");
        let shown: Vec<_> = events.try_iter().collect();
        if routed {
            let mut gate = MainDisplay::new();
            let limit = baseline.next_seq.get() + 32;
            let overflow_head =
                orca_runtime::surface::JsonlSurfaceCommitLedger::new(path, baseline)
                    .recover_batches()
                    .unwrap()
                    .committed
                    .into_iter()
                    .find(|batch| batch.cursor_after.next_seq.get() > limit)
                    .unwrap()
                    .cursor_after
                    .clone();
            let mut diagnostic_cursor = None;
            let mut ended = false;
            for event in shown {
                let mut must_pass = false;
                if let TuiEvent::SurfaceDelivery(delivery) = &event {
                    match &delivery.1 {
                        TuiEvent::Diagnostic(diagnostic) => {
                            must_pass = true;
                            assert!(
                                diagnostic
                                    .detail()
                                    .contains("typed TUI subscription gap requires SlowSubscriber")
                            );
                            assert_eq!(delivery.2.as_ref(), Some(&operation));
                            assert_eq!(delivery.0, overflow_head);
                            assert!(diagnostic_cursor.replace(delivery.0.clone()).is_none());
                        }
                        TuiEvent::SessionCompleted { status } => {
                            must_pass = true;
                            assert_eq!(delivery.2.as_ref(), Some(&operation));
                            assert_eq!(Some(&delivery.0), diagnostic_cursor.as_ref());
                            assert_eq!(status, "failed");
                            assert!(!ended);
                            ended = true;
                        }
                        _ => {}
                    }
                }
                let accepted = gate.send(event);
                assert!(!must_pass || accepted);
            }
            assert!(ended);
            assert_eq!(gate.state.status, crate::types::AppStatus::Idle);
            assert_eq!(
                gate.state
                    .transcript
                    .messages
                    .iter()
                    .filter(|row| matches!(
                        row,
                        crate::transcript_state::ChatMessage::Diagnostic(_)
                    ))
                    .count(),
                1
            );
            return;
        }
        assert_eq!(
            shown
                .iter()
                .filter(|event| matches!(event, TuiEvent::Error(_)))
                .count(),
            1
        );
        assert!(
            matches!(shown.last(),Some(TuiEvent::SessionCompleted{status}) if status=="failed")
        );
        let mut state = fresh_state();
        for event in shown {
            state.update(event);
        }
        assert_eq!(state.status, crate::types::AppStatus::Idle);
        assert_eq!(
            state
                .transcript
                .messages
                .iter()
                .filter(|message| matches!(
                    message,
                    crate::transcript_state::ChatMessage::Diagnostic(_)
                ))
                .count(),
            1
        );
    }

    fn overflow_delayed_reader(
        thread: &orca_runtime::runtime_host::RuntimeThreadHandle,
        capacity: u64,
    ) {
        use orca_runtime::surface::{DisplayText, SessionMetadataPatch};
        let mut revision = super::read_parent_snapshot(thread)
            .unwrap()
            .thread
            .metadata_revision;
        for index in 0..=capacity {
            revision = crate::surface_client::update_session_metadata(
                &thread.typed_surface(),
                revision,
                SessionMetadataPatch::SetTitle {
                    title: DisplayText::new(format!("gap {index}")),
                },
            )
            .unwrap()
            .metadata_revision;
        }
    }

    #[test]
    fn fix5_foregrounded_p_finalizes_once_after_backgrounding_p_then_q() {
        let fixture = ViewOnChild::start();
        let mut shown = MainDisplay::new();
        let (first, release_p) = background_turn(&fixture, &mut shown, "p");
        let (_, release_q) = background_turn(&fixture, &mut shown, "q");
        assert_eq!(shown.state.status, crate::types::AppStatus::Idle);
        let owner = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let task = first.task_id.unwrap();
        let worker = std::thread::spawn(move || {
            crate::surface_client::foreground_task(&owner, task.as_str(), &control, &sender)
        });
        assert!(
            fixture
                .control
                .wait_for_surface_active(std::time::Duration::from_secs(10))
        );
        std::fs::write(release_p, "").unwrap();
        worker.join().unwrap().unwrap();
        let mut finalized = 0;
        for event in events.try_iter() {
            let payload = event.clone().into_test_payload();
            assert!(!matches!(
                payload,
                TuiEvent::TurnStarted { .. } | TuiEvent::RuntimeTurnStarted { .. }
            ));
            let terminal = matches!(&event,TuiEvent::SurfaceDelivery(item) if item.2.as_ref()==Some(&first.operation_id) && matches!(&item.1,TuiEvent::SessionCompleted {status} if status=="success"));
            let previous = shown.state.last_completed_at;
            let accepted = shown.send(event);
            if terminal && accepted {
                assert_ne!(shown.state.last_completed_at, previous);
                assert!(shown.state.transcript.assistant_stream_tail.is_none());
                finalized += 1;
            }
        }
        std::fs::write(release_q, "").unwrap();
        fixture.shutdown();
        assert_eq!(finalized, 1, "P must pass both lifecycle gates");
        assert_eq!(shown.state.status, crate::types::AppStatus::Idle);
    }

    struct MainDisplay {
        route: std::sync::Arc<std::sync::Mutex<crate::attachment_routing::AttachmentRouting>>,
        state: crate::types::AppState,
    }

    impl MainDisplay {
        fn new() -> Self {
            let main = SessionAttachmentId::new(1);
            let mut state = fresh_state();
            state.active_session_attachment = Some(main);
            Self {
                route: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::attachment_routing::AttachmentRouting::new(main),
                )),
                state,
            }
        }

        fn send(&mut self, event: TuiEvent) -> bool {
            let (sender, events) = crossbeam_channel::unbounded();
            self.route
                .lock()
                .unwrap()
                .route(&sender, SessionAttachmentId::new(1), event);
            self.apply(events)
        }

        fn apply(&mut self, events: crossbeam_channel::Receiver<TuiEvent>) -> bool {
            let mut accepted = false;
            for event in events.try_iter() {
                if let Some(event) =
                    crate::attachment_routing::accept_attached_tui_event(&mut self.state, event)
                        .unwrap()
                {
                    self.state.update(event);
                    accepted = true;
                }
            }
            accepted
        }

        fn reproject(&mut self, fixture: &ViewOnChild, snapshot: &super::SurfaceSnapshot) {
            let (sender, events) = crossbeam_channel::unbounded();
            let main = SessionAttachmentId::new(1);
            crate::attachment_routing::AttachmentRouting::switch_attachment_deferred(
                &self.route,
                &sender,
                main,
                Some(main),
            );
            super::project_returned_main(
                &fixture.parent,
                snapshot,
                main,
                &sender,
                &fixture.control,
                &self.route,
            )
            .unwrap();
            crate::attachment_routing::AttachmentRouting::release_deferred_parent_events(
                &self.route,
                &sender,
            );
            self.apply(events);
        }
    }

    fn background_turn(
        fixture: &ViewOnChild,
        shown: &mut MainDisplay,
        label: &str,
    ) -> (
        orca_runtime::surface::SurfaceBackgroundOperation,
        std::path::PathBuf,
    ) {
        background_turn_after(fixture, shown, label, false)
    }

    fn background_turn_after(
        fixture: &ViewOnChild,
        shown: &mut MainDisplay,
        label: &str,
        reproject: bool,
    ) -> (
        orca_runtime::surface::SurfaceBackgroundOperation,
        std::path::PathBuf,
    ) {
        let release = fixture._home.path().join(label);
        let prompt = format!("mock_stream_release_marker {}", release.display());
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(fixture._home.path().to_owned());
        let owner = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let worker = std::thread::spawn(move || {
            crate::surface_client::run(
                &owner,
                orca_runtime::runtime_host::HostedTurnRequest::new(prompt),
                config,
                &control,
                &sender,
            )
        });
        let mut started_owner = None;
        let id = loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            if let TuiEvent::SurfaceDelivery(delivery) = &event
                && matches!(delivery.1, TuiEvent::TurnStarted { .. })
            {
                started_owner = delivery.2.clone();
            }
            let streamed = matches!(event.clone().into_test_payload(), TuiEvent::MessageDelta(_));
            shown.send(event);
            if streamed && let Some(owner) = started_owner.take() {
                break owner;
            }
        };
        let baseline = reproject.then(|| {
            use orca_runtime::surface::{NonEmptyVec, RuntimeSettingsPatch};
            let effort = super::read_parent_snapshot(&fixture.parent)
                .unwrap()
                .settings
                .effective
                .reasoning_effort;
            crate::surface_client::update_settings(
                &fixture.parent.typed_surface(),
                NonEmptyVec::try_new(vec![RuntimeSettingsPatch::SetReasoning { effort }]).unwrap(),
            )
            .unwrap();
            let snapshot = super::read_parent_snapshot(&fixture.parent).unwrap();
            shown.reproject(fixture, &snapshot);
            assert!(shown.state.lifecycle_cursor.as_ref().unwrap().live);
            snapshot.cursor.next_seq
        });
        let (actions, received) = crossbeam_channel::unbounded();
        crate::running_actions::handle_running_shortcut(
            crate::shortcuts::RunningShortcut::BackgroundCurrentTurn,
            &mut shown.state,
            &actions,
        );
        assert!(matches!(
            received.recv().unwrap(),
            crate::protocol::UserAction::BackgroundCurrentTurn
        ));
        assert!(fixture.control.request_background_current());
        assert!(
            matches!(worker.join().unwrap().unwrap(),crate::hosted_runtime::TuiHostedOperationOutcome::Turn{status} if status=="backgrounded")
        );
        let mut released = None;
        for event in events.try_iter() {
            if let TuiEvent::SurfaceDelivery(delivery) = &event
                && matches!(delivery.1, TuiEvent::OperationReleased)
            {
                released = Some(delivery.0.next_seq);
            }
            shown.send(event);
        }
        if let Some(head) = baseline {
            assert!(released.unwrap() < head);
        }
        let background = super::read_parent_snapshot(&fixture.parent)
            .unwrap()
            .background_operations
            .into_iter()
            .find(|op| op.operation_id == id)
            .unwrap();
        (background, release)
    }

    #[test]
    fn fix5_typed_transferred_settles_the_foreground_request() {
        use orca_runtime::surface::*;
        let fixture = ViewOnChild::start();
        let (background, marker) =
            background_turn(&fixture, &mut MainDisplay::new(), "transfer-fence");
        let interaction_id =
            SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let key = format!("{interaction_id:?}");
        let patch = InteractionPatch::Transferred {
            interaction_id,
            expected_revision: InteractionRevision::try_new(1).unwrap(),
            next_revision: InteractionRevision::try_new(2).unwrap(),
            background_fence: background.fence.clone(),
            route: SurfaceInteractionRoute::Unassigned {
                epoch: ResponseRouteEpoch::try_new(2).unwrap(),
            },
        };
        let settlement = crate::surface_client::forwarded_transfer_for_test(
            &background,
            patch,
            &fixture.control,
        );
        std::fs::write(marker, "").unwrap();
        fixture.shutdown();
        crate::attachment_routing::tests::assert_settlement_releases(&key, settlement);
    }

    #[test]
    fn fix5_cancelled_compact_after_background_p_takes_typing() {
        compact_after_background(true, false);
    }

    #[test]
    fn fix6_cancelled_compact_after_reprojected_handoff_takes_typing() {
        compact_after_background(true, true);
    }

    #[test]
    fn fix6_successful_compact_after_reprojected_handoff_ends_idle() {
        compact_after_background(false, true);
    }

    fn compact_after_background(cancelled: bool, reproject: bool) {
        use orca_runtime::runtime_host::{RuntimeHost, ThreadOperationExecutor};
        struct HoldCompact;
        impl ThreadOperationExecutor for HoldCompact {
            fn run_turn(
                &self,
                thread: &mut orca_runtime::thread::RuntimeThread,
                request: &orca_runtime::runtime_host::HostedTurnRequest,
                generation: &orca_runtime::runtime_host::GenerationContext,
                events: &mut orca_core::event_schema::EventFactory,
                writer: &mut (dyn std::io::Write + Send),
                cancel: &orca_core::cancel::CancelToken,
            ) -> std::io::Result<orca_runtime::runtime_host::ThreadOperationOutcome> {
                if request.operation_kind()
                    == &orca_runtime::runtime_host::HostedOperationKind::ManualCompaction
                {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !cancel.is_cancelled() {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                RuntimeHost::default_executor_for_test()
                    .run_turn(thread, request, generation, events, writer, cancel)
            }
        }
        let fixture = if cancelled {
            ViewOnChild::with_host(
                || RuntimeHost::start_with_executor(std::sync::Arc::new(HoldCompact)).unwrap(),
                |_| {},
            )
        } else {
            ViewOnChild::start()
        };
        let mut shown = MainDisplay::new();
        let (first, release) = background_turn_after(&fixture, &mut shown, "finish-p", reproject);
        std::fs::write(release, "").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !crate::surface_client::snapshot_operation_is_terminal(
            &super::read_parent_snapshot(&fixture.parent).unwrap(),
            &first.operation_id,
        ) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let thread = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let worker = std::thread::spawn(move || {
            crate::surface_client::manual_compact(&thread, &control, &sender)
        });
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            shown.send(event);
            if shown.state.status == crate::types::AppStatus::Compacting {
                break;
            }
        }
        if cancelled {
            assert!(fixture.control.interrupt_current().unwrap());
        }
        worker.join().unwrap().unwrap();
        let mut accepted = false;
        let mut compacted = false;
        for event in events.try_iter() {
            let terminal = matches!(&event,TuiEvent::SurfaceDelivery(item) if item.2.is_some() && matches!(&item.1,TuiEvent::SessionCompleted{status} if status== if cancelled {"cancelled"} else {"success"}));
            let completion = matches!(&event,TuiEvent::SurfaceDelivery(item) if item.2.is_some() && matches!(item.1,TuiEvent::Compacted{..}));
            let delivered = shown.send(event);
            compacted |= completion && delivered;
            accepted |= terminal && delivered;
        }
        fixture.shutdown();
        assert!(accepted, "the /compact terminal must pass both gates");
        if !cancelled {
            assert!(compacted, "owned Compacted was not accepted");
        }
        crate::status_key_actions::tests::assert_idle_takes_typing(&mut shown.state);
    }

    #[test]
    fn fix6_return_after_follow_projection_error_accepts_a_startless_terminal() {
        use orca_runtime::prompt_queue::{PromptQueueAction, PromptQueueInput};
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("orphan");
        let (sender, follows) = std::sync::mpsc::channel();
        fixture.parent.set_prompt_queue_surface_observer(
            std::sync::Arc::new(|_: &orca_core::event_schema::EventEnvelope| Ok(())),
            sender,
        );
        fixture
            .parent
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text(format!(
                    "mock_stream_release_marker {}",
                    marker.display()
                )),
            })
            .unwrap();
        let mut follow = follows
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        std::sync::Arc::make_mut(&mut follow.baseline.snapshot)
            .cursor
            .next_seq = orca_runtime::surface::SequenceNumber::new(1000);
        let (output, _events) = crossbeam_channel::unbounded();
        let err = crate::surface_client::follow_queue_operation(
            follow,
            &fixture.parent,
            &fixture.control,
            &output,
        )
        .unwrap_err();
        assert!(err.to_string().contains("CursorGap"), "{err}");
        let snapshot = fixture.wait_for(|snapshot| {
            (!snapshot.assistant_streams.is_empty() && snapshot.foreground_operation.is_some())
                .then_some(snapshot)
        });
        let mut shown = MainDisplay::new();
        shown.reproject(&fixture, &snapshot);
        std::fs::write(marker, "").unwrap();
        fixture.shutdown();
        let mut cursor = snapshot.cursor;
        cursor.next_seq = orca_runtime::surface::SequenceNumber::new(cursor.next_seq.get() + 1);
        let other = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        )
        .unwrap();
        assert!(
            !shown.state.lifecycle_cursor.as_ref().unwrap().live,
            "reprojection revived the absent reader"
        );
        assert!(
            shown.send(
                TuiEvent::SessionCompleted {
                    status: "success".into()
                }
                .at_operation_cursor(true, &cursor, &other)
            )
        );
    }

    #[test]
    fn fix6_closing_side_restores_mains_approval_turn_and_reply_once() {
        use crate::hosted_side::{HostedSideAction, handle_hosted_side_action};
        let fixture = ViewOnChild::start();
        let mut shown = MainDisplay::new();
        let main = SessionAttachmentId::new(1);
        let (root, displayed) = crossbeam_channel::unbounded();
        let mut attachment = main;
        let mut thread = Some(fixture.parent.clone());
        let mut sender = root.clone();
        let mut side = None;
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(fixture._home.path().to_owned());
        let config = std::sync::Arc::new(std::sync::Mutex::new(config));
        let routing = shown.route.clone();
        let mut action = |action| {
            handle_hosted_side_action(
                action,
                &mut thread,
                &mut side,
                &fixture.host.handle(),
                &config,
                &Default::default(),
                &root,
                &mut sender,
                &mut attachment,
                &routing,
                &fixture.control,
                &Default::default(),
            )
        };
        action(HostedSideAction::Start { prompt: None });
        shown.apply(displayed.clone());
        let witness = fixture._home.path().join("witness.txt");
        std::fs::write(&witness, "before").unwrap();
        let marker = fixture._home.path().join("side-approval");
        let events = fixture.enqueue(format!("mock_stream_tool_release_marker {}",serde_json::json!({"marker":marker,"toolPrompt":"edit witness.txt :: before => after","retainText":true})));
        std::fs::write(marker, "").unwrap();
        let request = loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            routing.lock().unwrap().route(&root, main, event.clone());
            if matches!(&event,TuiEvent::SurfaceDelivery(item) if matches!(item.1,TuiEvent::ApprovalNeeded{..}))
            {
                break event;
            }
        };
        shown.apply(displayed.clone());
        let snapshot = super::read_parent_snapshot(&fixture.parent).unwrap();
        let turn = snapshot
            .foreground_operation
            .unwrap()
            .generations
            .last()
            .unwrap()
            .logical_turn_id
            .to_string();
        action(HostedSideAction::Close);
        routing.lock().unwrap().route(&root, main, request);
        let mut approvals = 0;
        for event in displayed.try_iter() {
            if let Some(event) =
                crate::attachment_routing::accept_attached_tui_event(&mut shown.state, event)
                    .unwrap()
            {
                approvals += usize::from(matches!(event, TuiEvent::ApprovalNeeded { .. }));
                shown.state.update(event);
            }
        }
        assert_eq!(approvals, 1);
        assert_eq!(shown.state.status, crate::types::AppStatus::WaitingApproval);
        assert_eq!(shown.state.runtime_turn_id.as_deref(), Some(turn.as_str()));
        let key = shown
            .state
            .approval_dialog
            .as_ref()
            .unwrap()
            .interaction
            .clone()
            .unwrap();
        let prefix = "Mock slow tool stream started.";
        assert_eq!(
            format!("{:?}", shown.state.transcript.messages)
                .matches(prefix)
                .count(),
            1
        );
        assert!(
            fixture
                .control
                .respond_surface_interaction(
                    &key,
                    &crate::protocol::TuiInteractionResponse::Approval(false)
                )
                .unwrap()
        );
        fixture.wait_for(|snapshot| {
            snapshot
                .operation_history
                .iter()
                .any(|op| op.terminal.is_some())
                .then_some(())
        });
        assert_eq!(std::fs::read_to_string(witness).unwrap(), "before");
        fixture.shutdown();
    }

    #[test]
    fn fix5_drain_error_releases_its_owner_for_a_startless_terminal() {
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("fail-drain");
        let path = orca_runtime::history::SessionStore::new()
            .load_session(fixture.parent.thread_id())
            .unwrap()
            .path;
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(fixture._home.path().to_owned());
        let owner = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let prompt = format!("mock_stream_release_marker {}", marker.display());
        let worker = std::thread::spawn(move || {
            crate::surface_client::run(
                &owner,
                orca_runtime::runtime_host::HostedTurnRequest::new(prompt),
                config,
                &control,
                &sender,
            )
        });
        let mut shown = MainDisplay::new();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let prefix = matches!(event.clone().into_test_payload(), TuiEvent::MessageDelta(_));
            shown.send(event);
            if prefix {
                break;
            }
        }
        assert_eq!(shown.state.status, crate::types::AppStatus::Running);
        orca_runtime::surface::JsonlSurfaceCommitLedger::inject_terminal_append_failure_once(path);
        std::fs::write(marker, "").unwrap();
        assert!(
            worker
                .join()
                .unwrap()
                .err()
                .expect("failed drain")
                .to_string()
                .contains("requires recovery")
        );
        for event in events.try_iter() {
            shown.send(event);
        }
        assert!(shown.state.last_completed_at.is_none());
        let cursor = super::read_parent_snapshot(&fixture.parent).unwrap().cursor;
        let other = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        )
        .unwrap();
        fixture.control.unbind_view();
        drop(fixture.view.close());
        fixture.child.shutdown().unwrap();
        assert!(fixture.parent.shutdown().is_err());
        assert!(fixture.host.shutdown().is_err());
        assert!(
            shown.send(
                TuiEvent::SessionCompleted {
                    status: "success".into()
                }
                .at_operation_cursor(true, &cursor, &other)
            )
        );
        assert!(shown.state.last_completed_at.is_some());
    }

    #[test]
    fn fix4_foregrounded_live_turn_ends_without_another_start() {
        use crate::surface_client::{foreground_task, run};
        use orca_runtime::runtime_host::HostedTurnRequest;
        use orca_runtime::surface::SurfaceOperationId;
        use std::time::Duration;
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("foreground-release");
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(fixture._home.path().to_owned());
        let owner = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, visible) = crossbeam_channel::unbounded();
        let prompt = format!("mock_stream_release_marker {}", marker.display());
        let worker = std::thread::spawn(move || {
            run(
                &owner,
                HostedTurnRequest::new(prompt),
                config,
                &control,
                &sender,
            )
        });
        let timeout = Duration::from_secs(10);
        while !matches!(
            visible.recv_timeout(timeout).unwrap().into_test_payload(),
            TuiEvent::MessageDelta(_)
        ) {}
        assert!(fixture.control.request_background_current());
        assert!(
            matches!(worker.join().unwrap().unwrap(), crate::hosted_runtime::TuiHostedOperationOutcome::Turn {status} if status=="backgrounded")
        );
        let snapshot = super::read_parent_snapshot(&fixture.parent).unwrap();
        let task = snapshot.background_operations[0]
            .task_id
            .as_ref()
            .unwrap()
            .as_str()
            .to_owned();
        let previous =
            SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
        let mut state = fresh_state();
        state.update(
            TuiEvent::TurnStarted {
                turn: 1,
                task: None,
                started_at_ms: None,
            }
            .at_operation_cursor(true, &snapshot.cursor, &previous),
        );
        state.update(
            TuiEvent::SessionCompleted {
                status: "success".into(),
            }
            .at_operation_cursor(true, &snapshot.cursor, &previous),
        );
        state.set_status(crate::types::AppStatus::Running);
        let owner = fixture.parent.typed_surface();
        let control = fixture.control.clone();
        let (sender, events) = crossbeam_channel::unbounded();
        let foreground =
            std::thread::spawn(move || foreground_task(&owner, &task, &control, &sender));
        assert!(fixture.control.wait_for_surface_active(timeout));
        std::fs::write(marker, "").unwrap();
        foreground.join().unwrap().unwrap();
        let mut ended = false;
        for event in events.try_iter() {
            if let TuiEvent::SurfaceDelivery(item) = &event {
                assert!(!matches!(
                    item.1,
                    TuiEvent::TurnStarted { .. } | TuiEvent::RuntimeTurnStarted { .. }
                ));
                ended |= item.2.is_some()
                    && matches!(&item.1,TuiEvent::SessionCompleted{status} if status=="success");
            }
            state.update(event);
        }
        fixture.shutdown();
        assert!(ended);
        assert_eq!(state.status, crate::types::AppStatus::Idle);
    }

    #[test]
    fn a_delayed_gapped_queue_reader_does_not_block_the_next_follow() {
        delayed_reader_does_not_block_the_next_follow(true, false);
    }

    #[test]
    fn fix4_delayed_healthy_reader_delivers_everything_after_the_second_finishes() {
        delayed_reader_does_not_block_the_next_follow(false, false);
    }

    #[test]
    fn fix5_gap_read_failure_keeps_the_successful_receipt() {
        delayed_reader_does_not_block_the_next_follow(true, true);
    }

    fn delayed_reader_does_not_block_the_next_follow(gap: bool, fail_read: bool) {
        use orca_runtime::{
            prompt_queue::{PromptQueueAction, PromptQueueInput},
            runtime_host::RuntimeHost,
        };
        use std::{
            sync::{Arc, mpsc},
            time::Duration,
        };
        let timeout = Duration::from_secs(10);
        let home = crate::test_support::isolate_orca_home();
        let host = RuntimeHost::start_with_subscriber_limit_for_test(64).unwrap();
        let mut config = crate::test_support::test_run_config();
        config.history_mode = orca_core::config::HistoryMode::Record;
        config.cwd = Some(home.path().to_owned());
        let thread = host.start_thread(config, "two follows").unwrap();
        let (send, follows) = mpsc::channel();
        thread.set_prompt_queue_surface_observer(
            Arc::new(|_: &orca_core::event_schema::EventEnvelope| Ok(())),
            send,
        );
        let (start, started) = mpsc::channel();
        let (release, gate) = crossbeam_channel::bounded(1);
        let (finish, finished) = mpsc::channel();
        let (display, events) = crossbeam_channel::unbounded();
        let ordinal = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = thread.clone();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let dispatcher = std::thread::spawn(move || {
            crate::hosted_session::dispatch_queue_follows(
                follows,
                move |follow| {
                    let index = ordinal.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if index == 0 {
                        start.send(follow.operation.clone()).unwrap();
                        gate.recv_timeout(timeout).expect("release first reader");
                    }
                    crate::surface_client::follow_queue_operation_with_read(
                        follow,
                        &owner,
                        &control,
                        &display,
                        |thread| {
                            if fail_read && index == 0 {
                                Err(std::io::Error::other("final snapshot unavailable"))
                            } else {
                                crate::surface_client::read_snapshot(thread)
                            }
                        },
                    )
                    .unwrap();
                    finish.send(index).unwrap();
                },
                &crossbeam_channel::unbounded().0,
            );
        });
        let add = |prompt: &str| {
            thread
                .prompt_queue(PromptQueueAction::Add {
                    input: PromptQueueInput::text(prompt),
                })
                .unwrap()
        };
        let marker = home.path().join("finish-first");
        add(&format!("mock_stream_release_marker {}", marker.display()));
        let first = started.recv_timeout(timeout).unwrap();
        if gap {
            overflow_delayed_reader(&thread, 64);
        }
        std::fs::write(marker, "").unwrap();
        let deadline = std::time::Instant::now() + timeout;
        while !crate::surface_client::snapshot_operation_is_terminal(
            &super::read_parent_snapshot(&thread).unwrap(),
            &first,
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "first operation did not end"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        add("mock_stream_delay_ms 1");
        let shown = || {
            let batch = events.try_iter().collect::<Vec<_>>();
            let terminals = batch
                .iter()
                .filter(
                    |event| matches!(event,TuiEvent::SessionCompleted{status} if status=="success"),
                )
                .count();
            let text: String = batch
                .iter()
                .filter_map(|event| match event {
                    TuiEvent::MessageDelta(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let gaps = batch.iter().filter(|event| matches!(event, TuiEvent::Error(message) if message == "typed TUI subscription gap requires SlowSubscriber")).count();
            if fail_read && gaps == 1 {
                assert_eq!(batch.iter().filter(|event|matches!(event,TuiEvent::Error(message) if message.contains("final snapshot unavailable"))).count(),1);
                assert!(!batch.iter().any(
                    |event| matches!(event,TuiEvent::SessionCompleted {status} if status=="failed")
                ));
            }
            (terminals, text, gaps)
        };
        let index = finished
            .recv_timeout(timeout)
            .expect("second terminal while first reader held");
        let (terminals, second_text, gaps) = shown();
        assert_eq!((index, terminals, gaps), (1, 1, 0));
        assert_eq!(
            second_text,
            "Mock slow stream started.Mock slow stream completed."
        );
        release.send(()).unwrap();
        let index = finished
            .recv_timeout(timeout)
            .expect("first reader finishes after release");
        let (terminals, text, gaps) = shown();
        assert_eq!((index, terminals, gaps), (0, 1, usize::from(gap)));
        if !gap {
            assert_eq!(
                text,
                "Mock release-marker stream started.Mock release-marker stream completed."
            );
        }
        assert_eq!(
            text.matches("Mock release-marker stream completed.")
                .count(),
            usize::from(!fail_read),
            "{text}"
        );
        host.shutdown().unwrap();
        dispatcher.join().unwrap();
    }

    #[test]
    fn queued_terminal_append_failure_ends_the_live_and_returned_row_until_cold_resume() {
        use orca_runtime::{
            history::SessionStore,
            prompt_queue::{PromptQueueAction, PromptQueueInput},
            runtime_host::RuntimeHost,
            surface::{JsonlSurfaceCommitLedger, OperationPhase},
        };
        let fixture = ViewOnChild::start();
        let id = fixture.parent.thread_id().to_owned();
        let path = SessionStore::new().load_session(&id).unwrap().path;
        let marker = fixture._home.path().join("fail-terminal");
        let events = fixture.enqueue(format!("mock_stream_release_marker {}", marker.display()));
        let mut state = fresh_state();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let opened = matches!(&event, TuiEvent::SurfaceDelivery(item) if matches!(item.1,TuiEvent::MessageDelta(_)));
            state.update(event);
            if opened {
                break;
            }
        }
        assert_eq!(state.status, crate::types::AppStatus::Running);
        JsonlSurfaceCommitLedger::inject_terminal_append_failure_once(path);
        let second = "held behind recovery";
        fixture
            .parent
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text(second),
            })
            .unwrap();
        std::fs::write(marker, "").unwrap();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            assert!(
                !matches!(&event,TuiEvent::SurfaceDelivery(item) if matches!(item.1, TuiEvent::SessionCompleted{ .. }))
            );
            let failed = matches!(&event, TuiEvent::SurfaceDelivery(item) if matches!(&item.1,TuiEvent::TerminalRecoveryNeeded(message) if message=="typed TUI terminal commit requires recovery"));
            state.update(event);
            if failed {
                break;
            }
        }
        for mut shown in [state, returned_state(&fixture)] {
            let frame = crate::test_support::frame_string(&mut shown, 120, 40);
            assert_eq!(shown.status, crate::types::AppStatus::Idle, "{frame}");
            assert_eq!(frame.matches("requires recovery").count(), 1, "{frame}");
        }
        let snapshot = super::read_parent_snapshot(&fixture.parent).unwrap();
        let failed = snapshot.foreground_operation.unwrap();
        assert!(
            matches!(failed.phase, OperationPhase::Finalizing { .. }) && failed.terminal.is_none()
        );
        let queue = fixture
            .parent
            .prompt_queue(PromptQueueAction::List)
            .unwrap();
        assert!(queue.items.iter().any(|item| item.input.text == second));
        assert!(
            !queue
                .running_item()
                .is_some_and(|item| item.input.text == second)
        );
        fixture.control.unbind_view();
        drop(fixture.view.close());
        fixture.child.shutdown().unwrap();
        assert!(fixture.parent.shutdown().is_err());
        assert!(fixture.host.shutdown().is_err());
        drop((fixture.parent, fixture.child, fixture.view, fixture.control));
        let host = RuntimeHost::start().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(fixture._home.path().to_owned());
        config.history_mode = orca_core::config::HistoryMode::Resume(id);
        let resumed = host.start_thread(config, "cold resume").unwrap();
        assert!(
            super::read_parent_snapshot(&resumed)
                .unwrap()
                .operation_history
                .iter()
                .any(|op| op.operation_id == failed.operation_id && op.terminal.is_some())
        );
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (sender, visible) = crossbeam_channel::unbounded();
        crate::hosted_session::announce_runtime_ready(&resumed, &sender, &control);
        let fresh = "fresh queued message after recovery";
        let queue = resumed
            .prompt_queue(PromptQueueAction::Add {
                input: PromptQueueInput::text(fresh),
            })
            .unwrap();
        let fresh_turn = queue
            .items
            .iter()
            .find(|item| item.input.text == fresh)
            .unwrap()
            .client_user_message_id
            .turn_id();
        if queue.paused {
            resumed
                .prompt_queue(PromptQueueAction::Start {
                    expected_revision: queue.revision,
                })
                .unwrap();
        }
        while !matches!(visible.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), TuiEvent::RuntimeTurnStarted {turn_id} if turn_id == fresh_turn.as_str())
        {
        }
        while !matches!(visible.recv_timeout(std::time::Duration::from_secs(10)).unwrap(), TuiEvent::SessionCompleted {status} if status=="success")
        {
        }
        host.shutdown().unwrap();
    }

    #[test]
    fn another_attachment_cannot_background_a_live_followed_turn() {
        use orca_runtime::surface::{
            AttachResult, BackgroundTarget, FreshAttachRequest, SurfaceAttachmentRole,
            SurfaceCapability, SurfaceClientCommandError, SurfaceRequestId,
        };
        let fixture = ViewOnChild::start();
        let marker = fixture._home.path().join("finish-provider");
        let events = fixture.enqueue(format!("mock_stream_release_marker {}", marker.display()));
        let next = || {
            events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        };
        while !matches!(next(),TuiEvent::SurfaceDelivery(item) if matches!(&item.1,TuiEvent::MessageDelta(text) if text.contains("started")))
        {
        }
        let surface = fixture.parent.surface();
        let AttachResult::FreshAttached { attachment } = surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: [
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ControlBoundOperation,
            ]
            .into(),
            interaction_capabilities: Default::default(),
        }) else {
            panic!("control attachment")
        };
        let fence = attachment
            .baseline
            .snapshot
            .foreground_operation
            .as_ref()
            .unwrap()
            .generations
            .last()
            .unwrap()
            .fence
            .clone();
        assert!(matches!(
            attachment.client.transfer_background(
                SurfaceRequestId::new(),
                BackgroundTarget::ActiveGeneration {
                    fence: fence.clone()
                }
            ),
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        let snapshot = super::read_parent_snapshot(&fixture.parent).unwrap();
        assert_eq!(
            snapshot.foreground_operation.unwrap().operation_id,
            fence.operation_id
        );
        assert!(snapshot.background_operations.is_empty());
        super::detach_child_surface(&surface, &attachment.client);
        std::fs::write(marker, "").unwrap();
        while !matches!(next(), TuiEvent::SurfaceDelivery(item) if matches!(&item.1,TuiEvent::SessionCompleted{status} if status=="success"))
        {
        }
        fixture.shutdown();
    }

    #[test]
    fn failed_queued_turn_is_shown_once_after_return() {
        use crate::attachment_routing::{AttachmentRouting, accept_attached_tui_event};
        let fixture = ViewOnChild::start();
        let parent = SessionAttachmentId::new(1);
        let routing = std::sync::Arc::new(std::sync::Mutex::new(AttachmentRouting::new(parent)));
        let (root, displayed) = crossbeam_channel::unbounded();
        AttachmentRouting::switch_attachment(&routing, &root, parent.next(), Some(parent), false);
        let events = fixture.enqueue("mock_stream_closed_always queued-failure");
        let mut diagnostic = None;
        let mut start = None;
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            let terminal = matches!(&event,TuiEvent::SurfaceDelivery(item) if matches!(&item.1,TuiEvent::SessionCompleted{..}));
            if matches!(&event,TuiEvent::SurfaceDelivery(item) if matches!(item.1,TuiEvent::RuntimeTurnStarted{..}))
            {
                start = Some(event.clone());
            }
            if matches!(&event,TuiEvent::SurfaceDelivery(item) if matches!(item.1,TuiEvent::Diagnostic(_)))
            {
                diagnostic = Some(event.clone());
            }
            routing.lock().unwrap().route(&root, parent, event);
            if terminal {
                break;
            }
        }
        let diagnostic = diagnostic.expect("committed terminal diagnostic");
        let title = match &diagnostic {
            TuiEvent::SurfaceDelivery(item) => match &item.1 {
                TuiEvent::Diagnostic(diagnostic) => diagnostic.title(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let snapshot = super::snapshot_for_return(&routing, &root, parent, || {
            super::read_parent_snapshot(&fixture.parent)
        })
        .unwrap();
        super::project_returned_main(
            &fixture.parent,
            &snapshot,
            parent,
            &root,
            &fixture.control,
            &routing,
        )
        .unwrap();
        AttachmentRouting::release_deferred_parent_events(&routing, &root);
        routing.lock().unwrap().route(&root, parent, diagnostic);
        routing
            .lock()
            .unwrap()
            .route(&root, parent, start.expect("committed logical start"));
        let (actions, _) = crossbeam_channel::unbounded();
        let mut state =
            crate::types::AppState::new(actions, "test".into(), "mock".into(), "/tmp".into());
        for event in displayed.try_iter() {
            if let Ok(Some(event)) = accept_attached_tui_event(&mut state, event) {
                state.update(event);
            }
        }
        let frame = crate::test_support::frame_string(&mut state, 120, 40);
        assert_eq!(frame.matches(title).count(), 1, "{frame}");
        assert_eq!(state.status, crate::types::AppStatus::Idle);
        fixture.shutdown();
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
        let (sender, screen) = crossbeam_channel::unbounded();
        let (actions, source) = crossbeam_channel::unbounded();
        let (_dispatcher, _backlog) = crate::action_dispatcher::TuiActionDispatcher::spawn(
            source,
            sender,
            fixture.control.clone(),
            4,
            4,
        )
        .unwrap();
        actions
            .send(crate::protocol::UserAction::StopTask {
                task_id: task.task_id.as_str().into(),
            })
            .unwrap();
        let projection = loop {
            match fixture
                ._events
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            {
                TuiEvent::SurfaceProjectionSynced(projection)
                    if projection
                        .workflow_tasks
                        .iter()
                        .any(|row| row.id == task.task_id.as_str()) =>
                {
                    break projection;
                }
                TuiEvent::Notice(text) => panic!("stop notice went to main: {text}"),
                _ => (),
            }
        };
        let label = crate::background_tasks::task_notice_label(
            &projection.workflow_tasks,
            task.task_id.as_str(),
        );
        assert!(
            matches!(screen.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), TuiEvent::Notice(text) if text==format!("Stopping {label}."))
        );
        assert!(
            !screen
                .try_iter()
                .any(|event| matches!(event, TuiEvent::SurfaceProjectionSynced(_)))
        );
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
                agent_lines: Default::default(),
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
            Self::configured(|_| {})
        }

        fn configured(configure: impl FnOnce(&mut orca_core::config::RunConfig)) -> Self {
            let home = crate::test_support::isolate_orca_home();
            let mut config = crate::test_support::test_run_config();
            config.cwd = Some(home.path().to_path_buf());
            config.history_mode = orca_core::config::HistoryMode::Record;
            configure(&mut config);
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

    #[test]
    fn a_uuid_sync_batch_has_no_launch_item_live_or_rebuilt() {
        use crate::transcript_state::ChatMessage;
        let child = ChildThread::configured(|config| {
            config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto
        });
        child.host.handle().keep_subagents_in_process();
        let (sender, events) = crossbeam_channel::unbounded();
        run_child_turn(
            &child.thread,
            child.config.clone(),
            "subagent batch schema_fail",
            &sender,
        );
        let snapshot = super::read_parent_snapshot(&child.thread).unwrap();
        let requests: Vec<_> = snapshot
            .tools
            .iter()
            .filter(|tool| tool.request.name.as_str() == "subagent")
            .collect();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|tool| tool.result.is_none()));
        assert!(!snapshot.items.iter().any(|item| matches!(
            item,
            orca_runtime::surface::SurfaceItem::ToolResultMessage { .. }
        )));
        assert_eq!(snapshot.subagents.len(), 2);
        let batch = &snapshot.subagents[0].batch_id;
        uuid::Uuid::parse_str(batch.as_str()).unwrap();
        assert!(
            snapshot
                .subagents
                .iter()
                .all(|agent| &agent.batch_id == batch && agent.batch_size == 2)
        );
        let mut state = crate::types::AppState::new(
            crossbeam_channel::unbounded().0,
            "test".into(),
            "mock".into(),
            "/tmp".into(),
        );
        for event in events.try_iter() {
            state.update(event.into_test_payload());
        }
        let rows = |messages: &[ChatMessage]| {
            messages
                .iter()
                .filter_map(|message| match message {
                    ChatMessage::System { text, .. } if text.starts_with("Delegating") => {
                        Some("launch")
                    }
                    ChatMessage::System { text, .. } if text.starts_with("Agent completed:") => {
                        Some("success")
                    }
                    ChatMessage::System { text, .. } if text.starts_with("Agent failed:") => {
                        Some("failure")
                    }
                    ChatMessage::Assistant(text) | ChatMessage::AssistantChunk { text, .. }
                        if text.contains("Mock completed after tool execution.") =>
                    {
                        Some("reply")
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let live = rows(&state.transcript.messages);
        let rebuilt =
            rows(&crate::surface_projection::history_messages_from_surface_snapshot(&snapshot));
        assert_eq!(live.len(), 4, "{live:?}");
        assert_eq!((live[0], live[3]), ("launch", "reply"));
        assert_eq!(&rebuilt[..2], ["reply", "launch"]);
        for ends in [&live[1..3], &rebuilt[2..]] {
            assert_eq!(ends.len(), 2);
            assert!(ends.contains(&"success") && ends.contains(&"failure"));
        }
        child.shutdown();
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
