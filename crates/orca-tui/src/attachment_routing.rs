use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;

use crate::types::{AppState, AttachedTuiEvent, SessionAttachmentId, SideParentStatus, TuiEvent};

#[derive(Default)]
pub(crate) struct AttachmentRouting {
    active: Option<SessionAttachmentId>,
    parent_while_side: Option<SessionAttachmentId>,
    pending_parent_interactions: Vec<(SessionAttachmentId, TuiEvent)>,
    deferred_parent_attachment: Option<SessionAttachmentId>,
    deferred_parent_events: Vec<TuiEvent>,
}

impl AttachmentRouting {
    pub(crate) fn new(active: SessionAttachmentId) -> Self {
        Self {
            active: Some(active),
            parent_while_side: None,
            pending_parent_interactions: Vec::new(),
            deferred_parent_attachment: None,
            deferred_parent_events: Vec::new(),
        }
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
                if send_attached_event(root_event_tx, attachment, event).is_err() {
                    break;
                }
            }
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
        routing.active = Some(attachment);
        routing.parent_while_side = parent_while_side;
        routing.deferred_parent_attachment = Some(attachment);
        routing.deferred_parent_events.clear();
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
        for (_, event) in std::mem::take(&mut routing.pending_parent_interactions) {
            if send_attached_event(root_event_tx, attachment, event).is_err() {
                return;
            }
        }
        for event in std::mem::take(&mut routing.deferred_parent_events) {
            if send_attached_event(root_event_tx, attachment, event.clone()).is_err() {
                return;
            }
            if let Some(status) = side_parent_status_for_event(&event) {
                let _ = root_event_tx.send(TuiEvent::SideParentStatusChanged(status));
            }
        }
    }
}

fn is_tui_interaction_event(event: &TuiEvent) -> bool {
    matches!(
        event,
        TuiEvent::ApprovalNeeded { .. }
            | TuiEvent::PermissionApprovalNeeded { .. }
            | TuiEvent::UserInputRequested { .. }
            | TuiEvent::McpElicitationRequested { .. }
    )
}

pub(crate) fn accept_attached_tui_event(
    state: &mut AppState,
    event: TuiEvent,
) -> Result<Option<TuiEvent>, ()> {
    let TuiEvent::Attached(attached) = event else {
        return Ok(Some(event));
    };
    let AttachedTuiEvent { attachment, event } = *attached;
    if matches!(event, TuiEvent::SessionAttachmentActivated) {
        state.active_session_attachment = attachment;
        return Ok(None);
    }
    if attachment.is_some() && attachment != state.active_session_attachment {
        return Err(());
    }
    Ok(Some(event))
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
    *attachment = attachment.next();
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
                    if routing.deferred_parent_attachment == Some(attachment) {
                        routing.deferred_parent_events.push(event);
                    } else if routing.parent_while_side == Some(attachment)
                        && routing.active != Some(attachment)
                    {
                        if is_tui_interaction_event(&event) {
                            routing
                                .pending_parent_interactions
                                .push((attachment, event.clone()));
                        }
                        if let Some(status) = side_parent_status_for_event(&event) {
                            let _ = root_event_tx.send(TuiEvent::SideParentStatusChanged(status));
                        }
                    } else if send_attached_event(&root_event_tx, attachment, event).is_err() {
                        break;
                    }
                    continue;
                }
                if send_attached_event(&root_event_tx, attachment, event).is_err() {
                    break;
                }
            }
        })
        .expect("spawn TUI attachment relay");
    event_tx
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

fn side_parent_status_for_event(event: &TuiEvent) -> Option<SideParentStatus> {
    match event {
        TuiEvent::TurnStarted { .. } => Some(SideParentStatus::Running),
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
mod tests {
    use std::time::Duration;

    use orca_core::cancel::OperationIdAllocator;
    use orca_core::cost_types::UsageTotals;

    use super::*;
    use crate::types::{TuiInteractionKey, TuiInteractionKind};

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
        for _ in 0..100 {
            if routing.lock().unwrap().deferred_parent_events.len() == 1 {
                break;
            }
            std::thread::yield_now();
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
}
