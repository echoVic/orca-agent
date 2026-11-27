use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;

use crate::protocol::SessionAttachmentId;
use crate::protocol::{AttachedTuiEvent, TuiEvent, TuiInteractionKey};
use crate::types::{AppState, SideParentStatus};

#[derive(Default)]
pub(crate) struct AttachmentRouting {
    active: Option<SessionAttachmentId>,
    next_attachment: u64,
    parent_while_side: Option<SessionAttachmentId>,
    pending_parent_interactions: Vec<(SessionAttachmentId, TuiEvent)>,
    deferred_parent_attachment: Option<SessionAttachmentId>,
    deferred_parent_events: Vec<TuiEvent>,
    restored: Option<RestoredParent>,
    parent_lifecycle: Option<crate::protocol::LifecycleDelivery>,
    parent_waiting: std::collections::BTreeSet<String>,
}

struct RestoredParent {
    attachment: SessionAttachmentId,
    cursor: orca_runtime::surface::SurfaceCursor,
    recovery: Option<orca_runtime::surface::SurfaceOperationId>,
    // Kept until the next return; request identities are unique per operation.
    requests: Vec<TuiInteractionKey>,
}

impl AttachmentRouting {
    fn covered(&self, attachment: SessionAttachmentId, event: &TuiEvent) -> bool {
        let restored = self
            .restored
            .as_ref()
            .filter(|parent| parent.attachment == attachment);
        let event = match event {
            TuiEvent::SurfaceDelivery(delivery) => {
                let (cursor, event, owner) = delivery.as_ref();
                if restored.is_some_and(|parent| {
                    let releases_reader = matches!(event, TuiEvent::OperationReleased)
                        && cursor == &parent.cursor
                        && self.parent_lifecycle.as_ref().is_some_and(|previous| owner.as_ref() == Some(&previous.owner));
                    let needs_recovery = matches!(event, TuiEvent::TerminalRecoveryNeeded(_))
                        && owner.is_some() && owner != &parent.recovery;
                    !releases_reader && !needs_recovery && parent.cursor.thread_id == cursor.thread_id
                        && parent.cursor.incarnation == cursor.incarnation
                        // Hydration delivers at the baseline cursor itself.
                        && cursor.next_seq <= parent.cursor.next_seq
                }) {
                    return true;
                }
                event
            }
            event => event,
        };
        interaction_key(event)
            .is_some_and(|key| restored.is_some_and(|parent| parent.requests.contains(key)))
    }

    pub(crate) fn snapshot_projected(
        &mut self,
        attachment: SessionAttachmentId,
        snapshot: &orca_runtime::surface::SurfaceSnapshot,
        requests: &[TuiEvent],
        recovery: Option<orca_runtime::surface::SurfaceOperationId>,
    ) {
        self.parent_lifecycle =
            snapshot
                .foreground_operation
                .as_ref()
                .map(|op| crate::protocol::LifecycleDelivery {
                    owner: op.operation_id.clone(),
                    cursor: snapshot.cursor.clone(),
                    live: op.terminal.is_none() && recovery.as_ref() != Some(&op.operation_id),
                });
        self.parent_waiting = requests
            .iter()
            .filter_map(interaction_key)
            .filter(|key| {
                snapshot.interactions.iter().any(|interaction| {
                    self.parent_lifecycle.as_ref().is_some_and(|parent| {
                        parent.live && parent.owner == interaction.fence.operation_id
                    }) && format!("{:?}", interaction.interaction_id) == key.request_id
                })
            })
            .map(|key| key.request_id.clone())
            .collect();
        self.pending_parent_interactions
            .retain(|(source, _)| *source != attachment);
        self.restored = Some(RestoredParent {
            attachment,
            cursor: snapshot.cursor.clone(),
            recovery,
            requests: requests
                .iter()
                .filter_map(interaction_key)
                .cloned()
                .collect(),
        });
    }

    pub(crate) fn new(active: SessionAttachmentId) -> Self {
        Self {
            active: Some(active),
            next_attachment: active.value().checked_add(1).unwrap_or(1),
            parent_while_side: None,
            pending_parent_interactions: Vec::new(),
            deferred_parent_attachment: None,
            deferred_parent_events: Vec::new(),
            restored: None,
            parent_lifecycle: None,
            parent_waiting: Default::default(),
        }
    }

    /// Allocate an attachment identity that has not been used by this TUI
    /// controller lifetime. Re-entering a child must not reuse its old id:
    /// late events from the retired sender would otherwise look current.
    pub(crate) fn allocate_next(routing: &Arc<Mutex<Self>>) -> SessionAttachmentId {
        let Ok(mut routing) = routing.lock() else {
            return SessionAttachmentId::new(1);
        };
        if routing.next_attachment == 0 {
            routing.next_attachment = 1;
        }
        let attachment = SessionAttachmentId::new(routing.next_attachment);
        routing.observe_attachment(attachment);
        attachment
    }

    fn observe_attachment(&mut self, attachment: SessionAttachmentId) {
        self.next_attachment = attachment
            .value()
            .checked_add(1)
            .map_or(self.next_attachment, |next| self.next_attachment.max(next));
    }

    pub(crate) fn switch_attachment(
        routing: &Arc<Mutex<Self>>,
        root_event_tx: &mpsc::Sender<TuiEvent>,
        attachment: SessionAttachmentId,
        parent_while_side: Option<SessionAttachmentId>,
        replay_parent_interactions: bool,
    ) {
        let Ok(mut routing) = routing.lock() else {
            return;
        };
        if send_attached_event(
            root_event_tx,
            attachment,
            TuiEvent::SessionAttachmentActivated,
        )
        .is_err()
        {
            return;
        }
        if replay_parent_interactions {
            for (_, event) in std::mem::take(&mut routing.pending_parent_interactions) {
                if send_attached_event(root_event_tx, attachment, unpack(event)).is_err() {
                    break;
                }
            }
        }
        routing.observe_attachment(attachment);
        if let Some(parent_while_side) = parent_while_side {
            routing.observe_attachment(parent_while_side);
        }
        routing.active = Some(attachment);
        routing.parent_while_side = parent_while_side;
        routing.deferred_parent_attachment = None;
    }

    pub(crate) fn retire_attachment(routing: &Arc<Mutex<Self>>, attachment: SessionAttachmentId) {
        let Ok(mut routing) = routing.lock() else {
            return;
        };
        routing
            .pending_parent_interactions
            .retain(|(source, _)| *source != attachment);
        if routing.parent_while_side == Some(attachment) {
            routing.parent_while_side = None;
        }
        if routing
            .restored
            .as_ref()
            .is_some_and(|parent| parent.attachment == attachment)
        {
            routing.restored = None;
        }
    }

    pub(crate) fn switch_attachment_deferred(
        routing: &Arc<Mutex<Self>>,
        root_event_tx: &mpsc::Sender<TuiEvent>,
        attachment: SessionAttachmentId,
        parent_while_side: Option<SessionAttachmentId>,
    ) {
        let Ok(mut routing) = routing.lock() else {
            return;
        };
        if send_attached_event(
            root_event_tx,
            attachment,
            TuiEvent::SessionAttachmentActivated,
        )
        .is_err()
        {
            return;
        }
        routing.observe_attachment(attachment);
        if let Some(parent_while_side) = parent_while_side {
            routing.observe_attachment(parent_while_side);
        }
        routing.active = Some(attachment);
        routing.parent_while_side = parent_while_side;
        routing.defer_parent(attachment);
    }

    pub(crate) fn defer_parent(&mut self, attachment: SessionAttachmentId) {
        if self.deferred_parent_attachment != Some(attachment) {
            self.deferred_parent_events.clear();
            self.deferred_parent_attachment = Some(attachment);
        }
    }

    pub(crate) fn release_deferred_parent_events(
        routing: &Arc<Mutex<Self>>,
        root_event_tx: &mpsc::Sender<TuiEvent>,
    ) {
        let Ok(mut routing) = routing.lock() else {
            return;
        };
        let Some(attachment) = routing.deferred_parent_attachment.take() else {
            return;
        };
        for (source, event) in std::mem::take(&mut routing.pending_parent_interactions) {
            if routing.covered(source, &event) {
                continue;
            }
            if send_attached_event(root_event_tx, attachment, unpack(event)).is_err() {
                return;
            }
        }
        for event in std::mem::take(&mut routing.deferred_parent_events) {
            if routing.covered(attachment, &event) || !routing.accept_parent_delivery(&event) {
                continue;
            }
            let event = unpack(event);
            if send_attached_event(root_event_tx, attachment, event.clone()).is_err() {
                return;
            }
            if let Some(status) = side_parent_status_for_event(&event) {
                let _ = root_event_tx.send(TuiEvent::SideParentStatusChanged(status));
            }
        }
    }

    /// Abort a deferred projection and retain only parent interactions that
    /// must be replayed when the parent is visible again. Ordinary parent
    /// events are reconstructed from the next surface snapshot instead.
    pub(crate) fn cancel_deferred_parent_events(
        routing: &Arc<Mutex<Self>>,
        root_event_tx: &mpsc::Sender<TuiEvent>,
    ) {
        let Ok(mut routing) = routing.lock() else {
            return;
        };
        routing.deferred_parent_attachment = None;
        for event in std::mem::take(&mut routing.deferred_parent_events) {
            let source = routing
                .parent_while_side
                .unwrap_or_else(|| routing.active.unwrap_or(SessionAttachmentId::new(1)));
            if routing.covered(source, &event) || !routing.accept_parent_delivery(&event) {
                continue;
            }
            if interaction_key(payload(&event)).is_some() {
                routing
                    .pending_parent_interactions
                    .push((source, event.clone()));
            }
            if let Some(status) = side_parent_status_for_event(payload(&event)) {
                let _ = root_event_tx.send(TuiEvent::SideParentStatusChanged(status));
            }
        }
    }
}

fn unpack(event: TuiEvent) -> TuiEvent {
    match event {
        TuiEvent::SurfaceDelivery(delivery) if delivery.2.is_none() => delivery.1,
        event => event,
    }
}

fn payload(event: &TuiEvent) -> &TuiEvent {
    match event {
        TuiEvent::SurfaceDelivery(delivery) => &delivery.1,
        event => event,
    }
}

fn interaction_key(event: &TuiEvent) -> Option<&TuiInteractionKey> {
    match event {
        TuiEvent::ApprovalNeeded { key, .. }
        | TuiEvent::PermissionApprovalNeeded { key, .. }
        | TuiEvent::UserInputRequested { key, .. }
        | TuiEvent::McpElicitationRequested { key, .. } => Some(key),
        _ => None,
    }
}

fn background_tasks_from_event(
    event: &TuiEvent,
) -> Option<Vec<orca_core::task_types::BackgroundTaskSummary>> {
    match event {
        TuiEvent::SurfaceProjectionSynced(projection) => Some(projection.workflow_tasks.clone()),
        TuiEvent::WorkflowTasksUpdated(tasks) => Some(tasks.clone()),
        _ => None,
    }
}

pub(crate) fn accept_attached_tui_event(
    state: &mut AppState,
    event: TuiEvent,
) -> Result<Option<TuiEvent>, ()> {
    let TuiEvent::Attached(attached) = event else {
        return Ok(state.accept_surface_delivery(event));
    };
    let AttachedTuiEvent { attachment, event } = *attached;
    if matches!(event, TuiEvent::SessionAttachmentActivated) {
        state.active_session_attachment = attachment;
        return Ok(None);
    }
    if attachment.is_some() && attachment != state.active_session_attachment {
        return Err(());
    }
    Ok(state.accept_surface_delivery(event))
}

#[cfg(test)]
pub(crate) fn reduce_attached_tui_event(state: &mut AppState, event: AttachedTuiEvent) -> bool {
    match accept_attached_tui_event(state, TuiEvent::Attached(Box::new(event))) {
        Ok(Some(event)) => {
            state.update(event);
            true
        }
        Ok(None) => true,
        Err(()) => false,
    }
}

pub(crate) fn spawn_attached_event_sender(
    root_event_tx: mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
) -> mpsc::Sender<TuiEvent> {
    let _ = send_attached_event(
        &root_event_tx,
        attachment,
        TuiEvent::SessionAttachmentActivated,
    );
    spawn_attached_event_sender_with_routing(root_event_tx, attachment, None)
}

pub(crate) fn rotate_attached_event_sender(
    root_event_tx: &mpsc::Sender<TuiEvent>,
    attachment: &mut SessionAttachmentId,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    routing: Option<&Arc<Mutex<AttachmentRouting>>>,
) {
    *attachment = match routing {
        Some(routing) => AttachmentRouting::allocate_next(routing),
        None => attachment.next(),
    };
    *event_tx = match routing {
        Some(routing) => {
            let event_tx = spawn_attached_event_sender_with_routing(
                root_event_tx.clone(),
                *attachment,
                Some(routing.clone()),
            );
            AttachmentRouting::switch_attachment(routing, root_event_tx, *attachment, None, false);
            event_tx
        }
        None => spawn_attached_event_sender(root_event_tx.clone(), *attachment),
    };
}

pub(crate) fn spawn_attached_event_sender_with_routing(
    root_event_tx: mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
    routing: Option<Arc<Mutex<AttachmentRouting>>>,
) -> mpsc::Sender<TuiEvent> {
    let (event_tx, event_rx) = mpsc::bounded(crate::channels::TUI_EVENT_CAPACITY);
    std::thread::Builder::new()
        .name(format!("orca-tui-attachment-{}", attachment.value()))
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                if matches!(event, TuiEvent::SessionAttachmentActivated) {
                    continue;
                }
                if let Some(routing) = routing.as_ref() {
                    let Ok(mut routing) = routing.lock() else {
                        break;
                    };
                    if !routing.route(&root_event_tx, attachment, event) {
                        break;
                    }
                } else if send_attached_event(&root_event_tx, attachment, event).is_err() {
                    break;
                }
            }
        })
        .expect("spawn TUI attachment relay");
    event_tx
}

impl AttachmentRouting {
    fn accept_parent_delivery(&mut self, event: &TuiEvent) -> bool {
        if let TuiEvent::SurfaceRequestSettled(request) = payload(event) {
            self.parent_waiting.remove(request);
            self.pending_parent_interactions.retain(|(_, pending)| {
                interaction_key(payload(pending)).is_none_or(|key| &key.request_id != request)
            });
            return false;
        }
        if let TuiEvent::SurfaceDelivery(delivery) = event
            && let (cursor, event, Some(owner)) = delivery.as_ref()
            && !matches!(event, TuiEvent::Diagnostic(_) | TuiEvent::Compacted { .. })
        {
            if !event.accepts_lifecycle(
                self.parent_lifecycle.as_ref(),
                owner,
                cursor,
                !self.parent_waiting.is_empty(),
            ) {
                return false;
            }
            let delivery = event.lifecycle_delivery(owner.clone(), cursor.clone());
            if !delivery.live
                || self
                    .parent_lifecycle
                    .as_ref()
                    .is_none_or(|previous| previous.owner != *owner)
            {
                self.parent_waiting.clear();
            }
            if let Some(key) = interaction_key(event) {
                self.parent_waiting.insert(key.request_id.clone());
            }
            self.parent_lifecycle = Some(delivery);
        }
        true
    }

    pub(crate) fn route(
        &mut self,
        root_event_tx: &mpsc::Sender<TuiEvent>,
        attachment: SessionAttachmentId,
        event: TuiEvent,
    ) -> bool {
        if self.deferred_parent_attachment == Some(attachment) {
            self.deferred_parent_events.push(event);
            return true;
        }
        if self.covered(attachment, &event) {
            return true;
        }
        if (self.parent_while_side == Some(attachment)
            || self.parent_while_side.is_none() && self.active == Some(attachment))
            && !self.accept_parent_delivery(&event)
        {
            return true;
        }
        if self.parent_while_side == Some(attachment) && self.active != Some(attachment) {
            if let Some(tasks) = background_tasks_from_event(payload(&event)) {
                let _ = root_event_tx.send(TuiEvent::BackgroundTasksUpdated(tasks));
            }
            if interaction_key(payload(&event)).is_some() {
                self.pending_parent_interactions
                    .push((attachment, event.clone()));
            }
            if let Some(status) = side_parent_status_for_event(payload(&event)) {
                let _ = root_event_tx.send(TuiEvent::SideParentStatusChanged(status));
            }
            // The background key acts on main's turn from any
            // view, so its answer goes to the view on screen.
            // An answer that comes later (see the reducer's
            // arm) goes to the view on screen when it arrives,
            // and restores only that view's status; stage 2's
            // work on returning to main is to revisit this.
            if let TuiEvent::TurnKeptInForeground(notice) = event {
                let _ = root_event_tx.send(TuiEvent::TurnKeptInForeground(notice));
            }
        } else if send_attached_event(root_event_tx, attachment, unpack(event)).is_err() {
            return false;
        }
        true
    }
}

pub(crate) fn send_attached_event(
    root_event_tx: &mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
    event: TuiEvent,
) -> Result<(), mpsc::SendError<TuiEvent>> {
    root_event_tx.send(TuiEvent::Attached(Box::new(AttachedTuiEvent {
        attachment: Some(attachment),
        event,
    })))
}

pub(crate) fn side_parent_status_for_event(event: &TuiEvent) -> Option<SideParentStatus> {
    match payload(event) {
        TuiEvent::TerminalRecoveryNeeded(_) => Some(SideParentStatus::Failed),
        TuiEvent::TurnStarted { .. } | TuiEvent::RuntimeTurnStarted { .. } => {
            Some(SideParentStatus::Running)
        }
        TuiEvent::ApprovalNeeded { .. }
        | TuiEvent::PermissionApprovalNeeded { .. }
        | TuiEvent::McpElicitationRequested { .. } => Some(SideParentStatus::NeedsApproval),
        TuiEvent::UserInputRequested { .. } => Some(SideParentStatus::NeedsInput),
        TuiEvent::SessionCompleted { status } => match status.as_str() {
            "success" | "completed" => Some(SideParentStatus::Finished),
            "interrupted" | "cancelled" => Some(SideParentStatus::Interrupted),
            "failed" | "error" => Some(SideParentStatus::Failed),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use orca_core::cancel::OperationIdAllocator;
    use orca_core::cost_types::UsageTotals;

    use super::*;
    use crate::protocol::{TuiInteractionKey, TuiInteractionKind};

    fn approval(request_id: &str) -> TuiEvent {
        TuiEvent::ApprovalNeeded {
            key: TuiInteractionKey::new(
                OperationIdAllocator::new().allocate(),
                request_id,
                TuiInteractionKind::Approval,
            ),
            tool: "bash".to_string(),
            target: None,
            preview: None,
        }
    }

    fn receive(receiver: &mpsc::Receiver<TuiEvent>) -> TuiEvent {
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("routed TUI event")
    }

    pub(crate) fn assert_settlement_releases(key: &str, settlement: TuiEvent) {
        let mut route = AttachmentRouting::new(SessionAttachmentId::new(1));
        let owner = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        )
        .unwrap();
        let deliver = |route: &mut AttachmentRouting, event: TuiEvent| {
            route.accept_parent_delivery(&event.at_operation_cursor(
                true,
                &crate::surface_projection::test_surface_cursor(3),
                &owner,
            ))
        };
        let start = || TuiEvent::RuntimeTurnStarted {
            turn_id: "round".into(),
        };
        assert!(deliver(&mut route, approval(key)));
        assert!(!deliver(&mut route, start()));
        assert!(!deliver(&mut route, settlement));
        assert!(deliver(&mut route, start()));
    }

    fn approval_request_id(event: TuiEvent) -> String {
        let TuiEvent::Attached(attached) = event else {
            panic!("expected attached event");
        };
        let TuiEvent::ApprovalNeeded { key, .. } = attached.event else {
            panic!("expected approval event");
        };
        key.request_id
    }

    #[test]
    fn fix5_both_restored_requests_must_settle_before_the_next_round() {
        crate::hosted_child::tests::with_streaming_main(|_, mut snapshot| {
            use orca_runtime::surface::*;
            let main = SessionAttachmentId::new(1);
            let mut route = AttachmentRouting::new(main);
            let owner = &snapshot.foreground_operation.as_ref().unwrap().operation_id;
            let keys = ["first", "second"].map(|question| {
                let id =
                    SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
                let key = format!("{id:?}");
                snapshot.interactions.push(SurfaceInteractionView {
                    interaction_id: id,
                    revision: InteractionRevision::try_new(1).unwrap(),
                    fence: snapshot
                        .foreground_operation
                        .as_ref()
                        .unwrap()
                        .generations
                        .last()
                        .unwrap()
                        .fence
                        .clone(),
                    kind: SurfaceInteractionKind::UserInput,
                    request: SurfaceInteractionRequest::UserInput {
                        question: NonEmptyText::try_new(question).unwrap(),
                        suggestions: vec![],
                    },
                    route: SurfaceInteractionRoute::Unassigned {
                        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                    },
                    lifecycle: SurfaceInteractionLifecycle::Requested,
                    recovery_disposition: InteractionUnavailableDisposition::FailOperation,
                });
                key
            });
            let ui_operation = OperationIdAllocator::new().allocate();
            let restored = keys.each_ref().map(|key| TuiEvent::UserInputRequested {
                key: TuiInteractionKey::new(
                    ui_operation,
                    key.clone(),
                    TuiInteractionKind::UserInput,
                ),
                questionnaire: crate::protocol::TuiUserInputQuestionnaire::single(
                    "Question",
                    vec![],
                ),
            });
            route.snapshot_projected(main, &snapshot, &restored, None);
            let mut cursor = snapshot.cursor.clone();
            let mut deliver = |event: TuiEvent| {
                cursor.next_seq =
                    orca_runtime::surface::SequenceNumber::new(cursor.next_seq.get() + 1);
                route.accept_parent_delivery(&event.at_operation_cursor(true, &cursor, owner))
            };
            let start = || TuiEvent::RuntimeTurnStarted {
                turn_id: "round".into(),
            };
            assert!(!deliver(TuiEvent::SurfaceRequestSettled(keys[0].clone())));
            assert!(!deliver(start()), "second request is still pending");
            assert!(!deliver(TuiEvent::SurfaceRequestSettled(keys[1].clone())));
            assert!(deliver(start()));
        });
    }

    #[test]
    fn fix4_answered_approval_allows_the_same_operations_next_round_at_the_route() {
        let main = SessionAttachmentId::new(1);
        let mut route = AttachmentRouting::new(main.next());
        route.parent_while_side = Some(main);
        let (sender, events) = mpsc::unbounded();
        let operation = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        )
        .unwrap();
        for (seq, event) in [
            (1, approval("current")),
            (2, TuiEvent::SurfaceRequestSettled("other".into())),
            (
                3,
                TuiEvent::RuntimeTurnStarted {
                    turn_id: "blocked".into(),
                },
            ),
            (4, TuiEvent::SurfaceRequestSettled("current".into())),
            (
                5,
                TuiEvent::RuntimeTurnStarted {
                    turn_id: "next".into(),
                },
            ),
        ] {
            route.route(
                &sender,
                main,
                event.at_operation_cursor(
                    true,
                    &crate::surface_projection::test_surface_cursor(seq),
                    &operation,
                ),
            );
        }
        let statuses: Vec<_> = events
            .try_iter()
            .map(|event| match event {
                TuiEvent::SideParentStatusChanged(status) => status,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            statuses,
            [SideParentStatus::NeedsApproval, SideParentStatus::Running]
        );
    }

    #[test]
    fn fix4_route_accepts_an_unstarted_terminal_only_after_the_live_owner_ends() {
        let parent = SessionAttachmentId::new(1);
        let mut route = AttachmentRouting::new(parent.next());
        route.parent_while_side = Some(parent);
        let (sender, events) = mpsc::unbounded();
        let mut send = |seq, owner, event: TuiEvent| {
            route.route(
                &sender,
                parent,
                event.at_operation_cursor(
                    true,
                    &crate::surface_projection::test_surface_cursor(seq),
                    owner,
                ),
            );
            events.try_recv().ok().map(|event| match event {
                TuiEvent::SideParentStatusChanged(status) => status,
                other => panic!("{other:?}"),
            })
        };
        let op = || {
            orca_runtime::surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .unwrap()
        };
        let prior = op();
        let next = op();
        let start = || TuiEvent::RuntimeTurnStarted {
            turn_id: "prior".into(),
        };
        let end = || TuiEvent::SessionCompleted {
            status: "success".into(),
        };
        assert_eq!(send(2, &prior, start()), Some(SideParentStatus::Running));
        assert_eq!(
            send(3, &prior, approval("pending")),
            Some(SideParentStatus::NeedsApproval)
        );
        assert_eq!(send(4, &prior, start()), None);
        assert_eq!(send(5, &next, end()), None);
        assert_eq!(send(6, &prior, end()), Some(SideParentStatus::Finished));
        assert_eq!(send(1, &next, end()), None);
        assert_eq!(send(7, &next, end()), Some(SideParentStatus::Finished));
    }

    #[test]
    fn parent_interactions_replay_fifo_before_new_active_events() {
        let (root_tx, root_rx) = mpsc::unbounded();
        let parent = SessionAttachmentId::new(1);
        let side = parent.next();
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));
        let parent_tx = spawn_attached_event_sender_with_routing(
            root_tx.clone(),
            parent,
            Some(routing.clone()),
        );

        AttachmentRouting::switch_attachment(&routing, &root_tx, side, Some(parent), false);
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::Attached(attached)
                if attached.attachment == Some(side)
                    && matches!(attached.event, TuiEvent::SessionAttachmentActivated)
        ));

        parent_tx.send(approval("older")).unwrap();
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::SideParentStatusChanged(SideParentStatus::NeedsApproval)
        ));

        AttachmentRouting::switch_attachment(&routing, &root_tx, parent, Some(parent), true);
        parent_tx.send(approval("newer")).unwrap();

        assert!(matches!(
            receive(&root_rx),
            TuiEvent::Attached(attached)
                if attached.attachment == Some(parent)
                    && matches!(attached.event, TuiEvent::SessionAttachmentActivated)
        ));
        assert_eq!(approval_request_id(receive(&root_rx)), "older");
        assert_eq!(approval_request_id(receive(&root_rx)), "newer");
    }

    /// The background key acts on main's turn from an agent's view too, so
    /// the answer that main's turn stays reaches the view on screen.
    #[test]
    fn a_turn_kept_in_the_foreground_is_shown_while_main_is_behind_another_view() {
        const STAYS: &str =
            "This turn cannot move to the background while its subagents are running.";
        let (root_tx, root_rx) = mpsc::unbounded();
        let parent = SessionAttachmentId::new(1);
        let child = parent.next();
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));
        let parent_tx = spawn_attached_event_sender_with_routing(
            root_tx.clone(),
            parent,
            Some(routing.clone()),
        );
        AttachmentRouting::switch_attachment(&routing, &root_tx, child, Some(parent), false);
        let _ = receive(&root_rx);

        parent_tx
            .send(TuiEvent::TurnKeptInForeground(STAYS.to_string()))
            .unwrap();

        assert!(matches!(
            receive(&root_rx),
            TuiEvent::TurnKeptInForeground(message) if message == STAYS
        ));
        assert!(root_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn relay_cannot_publish_an_activation_outside_routing_authority() {
        let (root_tx, root_rx) = mpsc::unbounded();
        let parent = SessionAttachmentId::new(1);
        let side = parent.next();
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));
        let parent_tx = spawn_attached_event_sender_with_routing(
            root_tx.clone(),
            parent,
            Some(routing.clone()),
        );

        AttachmentRouting::switch_attachment(&routing, &root_tx, side, Some(parent), false);
        let _ = receive(&root_rx);
        parent_tx
            .send(TuiEvent::SessionAttachmentActivated)
            .unwrap();

        assert!(root_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn rotating_side_discards_interactions_queued_by_retired_attachment() {
        let (root_tx, root_rx) = mpsc::unbounded();
        let parent = SessionAttachmentId::new(1);
        let old_side = parent.next();
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));
        let old_side_tx = spawn_attached_event_sender_with_routing(
            root_tx.clone(),
            old_side,
            Some(routing.clone()),
        );

        AttachmentRouting::switch_attachment(&routing, &root_tx, parent, Some(old_side), false);
        let _ = receive(&root_rx);
        old_side_tx.send(approval("stale-side")).unwrap();
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::SideParentStatusChanged(SideParentStatus::NeedsApproval)
        ));
        for _ in 0..100 {
            if routing.lock().unwrap().pending_parent_interactions.len() == 1 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(routing.lock().unwrap().pending_parent_interactions.len(), 1);

        let mut new_side = old_side;
        let _new_side_tx =
            crate::hosted_side::rotate_side_event_sender(&root_tx, &mut new_side, &routing);
        assert_ne!(new_side, old_side);
        assert!(
            routing
                .lock()
                .unwrap()
                .pending_parent_interactions
                .is_empty()
        );

        AttachmentRouting::switch_attachment(&routing, &root_tx, new_side, Some(parent), false);
        let _ = receive(&root_rx);
        AttachmentRouting::switch_attachment_deferred(&routing, &root_tx, parent, Some(new_side));
        let _ = receive(&root_rx);
        AttachmentRouting::release_deferred_parent_events(&routing, &root_tx);

        assert!(root_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn deferred_parent_replay_waits_for_projection_barrier() {
        let (root_tx, root_rx) = mpsc::unbounded();
        let parent = SessionAttachmentId::new(1);
        let side = parent.next();
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));
        let parent_tx = spawn_attached_event_sender_with_routing(
            root_tx.clone(),
            parent,
            Some(routing.clone()),
        );

        AttachmentRouting::switch_attachment(&routing, &root_tx, side, Some(parent), false);
        let _ = receive(&root_rx);
        parent_tx.send(approval("older")).unwrap();
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::SideParentStatusChanged(SideParentStatus::NeedsApproval)
        ));

        AttachmentRouting::switch_attachment_deferred(&routing, &root_tx, parent, Some(parent));
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::Attached(attached)
                if attached.attachment == Some(parent)
                    && matches!(attached.event, TuiEvent::SessionAttachmentActivated)
        ));
        parent_tx.send(approval("newer")).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while routing.lock().unwrap().deferred_parent_events.len() < 1 {
            if std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(routing.lock().unwrap().deferred_parent_events.len(), 1);

        send_attached_event(
            &root_tx,
            parent,
            TuiEvent::SessionProjectionReset(Box::new(
                crate::surface_projection::SurfaceProjectionState {
                    cursor: crate::surface_projection::test_surface_cursor(1),
                    session_id: Some("parent".to_string()),
                    title: "Parent".to_string(),
                    usage_revision: 1,
                    usage: UsageTotals::default(),
                    context_revision: 1,
                    context_used_tokens: 0,
                    context_limit_tokens: 0,
                    workflow_tasks: Vec::new(),
                    current_goal: None,
                    foreground_operation_id: None,
                    recoverable_operation_id: None,
                    goal_presentation: None,
                    session_presentation: None,
                    mcp_catalog: Default::default(),
                },
            )),
        )
        .unwrap();
        AttachmentRouting::release_deferred_parent_events(&routing, &root_tx);

        assert!(matches!(
            receive(&root_rx),
            TuiEvent::Attached(attached)
                if matches!(attached.event, TuiEvent::SessionProjectionReset(_))
        ));
        assert_eq!(approval_request_id(receive(&root_rx)), "older");
        assert_eq!(approval_request_id(receive(&root_rx)), "newer");
        assert!(matches!(
            receive(&root_rx),
            TuiEvent::SideParentStatusChanged(SideParentStatus::NeedsApproval)
        ));
    }

    #[test]
    fn attachment_allocator_keeps_child_ids_monotonic_across_focus_cycles() {
        let parent = SessionAttachmentId::new(1);
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(parent)));

        let first_child = AttachmentRouting::allocate_next(&routing);
        let second_child = AttachmentRouting::allocate_next(&routing);

        assert_eq!(first_child.value(), 2);
        assert_eq!(second_child.value(), 3);
        assert_ne!(first_child, second_child);
    }

    #[test]
    fn a_turn_the_runtime_starts_marks_the_side_parent_running() {
        assert!(matches!(
            super::side_parent_status_for_event(&TuiEvent::RuntimeTurnStarted {
                turn_id: "turn_queued".to_string(),
            }),
            Some(SideParentStatus::Running)
        ));
    }
}
