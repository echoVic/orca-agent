use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;

use crate::types::{AppState, AttachedTuiEvent, SessionAttachmentId, SideParentStatus, TuiEvent};

#[derive(Default)]
pub(crate) struct AttachmentRouting {
    active: Option<SessionAttachmentId>,
    parent_while_side: Option<SessionAttachmentId>,
    pending_parent_interactions: Vec<TuiEvent>,
}

impl AttachmentRouting {
    pub(crate) fn new(active: SessionAttachmentId) -> Self {
        Self {
            active: Some(active),
            parent_while_side: None,
            pending_parent_interactions: Vec::new(),
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
            for event in std::mem::take(&mut routing.pending_parent_interactions) {
                if send_attached_event(root_event_tx, attachment, event).is_err() {
                    break;
                }
            }
        }
        routing.active = Some(attachment);
        routing.parent_while_side = parent_while_side;
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
                    if routing.parent_while_side == Some(attachment)
                        && routing.active != Some(attachment)
                    {
                        if is_tui_interaction_event(&event) {
                            routing.pending_parent_interactions.push(event.clone());
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

fn send_attached_event(
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
}
