//! Hosted side-parent ownership: shutdown ordering for the attached child
//! and parent actors, parent-status projection, active config selection,
//! and attached event-sender rotation. Extracted from `app.rs` (TUI
//! convergence slice 7).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;

use crate::attachment_routing::{
    AttachmentRouting, spawn_attached_event_sender, spawn_attached_event_sender_with_routing,
};
use crate::types::{SessionAttachmentId, SideParentStatus, TuiEvent};
use orca_core::config::RunConfig;
use orca_runtime::runtime_host::{RuntimeThreadHandle, RuntimeThreadState};

pub(crate) struct HostedSideParent {
    pub(crate) thread: RuntimeThreadHandle,
    pub(crate) event_tx: mpsc::Sender<TuiEvent>,
    pub(crate) attachment: SessionAttachmentId,
    pub(crate) side_thread: RuntimeThreadHandle,
    pub(crate) side_event_tx: mpsc::Sender<TuiEvent>,
    pub(crate) side_attachment: SessionAttachmentId,
    pub(crate) side_config: Arc<Mutex<RunConfig>>,
    pub(crate) parent_title: String,
}

pub(crate) fn shutdown_attached_side_on_controller_exit(side: HostedSideParent) {
    // The controller owns both actors while the attached child exists.
    // Always settle/join the child first, even when it is the visible
    // projection, then release the parent. No actor may be left behind on
    // TUI exit or allowed to publish late events.
    let _ = side
        .side_thread
        .shutdown_with_timeout(Duration::from_secs(5));
    let _ = side.thread.shutdown_with_timeout(Duration::from_secs(5));
}

pub(crate) fn side_parent_status_for_runtime_thread(
    thread: &RuntimeThreadHandle,
) -> SideParentStatus {
    match thread.state() {
        Ok(RuntimeThreadState::Running { .. }) => SideParentStatus::Running,
        Ok(RuntimeThreadState::Idle) => SideParentStatus::Idle,
        Ok(RuntimeThreadState::Unavailable) | Err(_) => SideParentStatus::Closed,
    }
}

pub(crate) fn hosted_config_for_active(
    side_parent: Option<&HostedSideParent>,
    thread: Option<&RuntimeThreadHandle>,
    main_config: &Arc<Mutex<RunConfig>>,
) -> Arc<Mutex<RunConfig>> {
    if let (Some(side), Some(active)) = (side_parent, thread)
        && active.thread_id() == side.side_thread.thread_id()
    {
        return side.side_config.clone();
    }
    main_config.clone()
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

pub(crate) fn rotate_side_event_sender(
    root_event_tx: &mpsc::Sender<TuiEvent>,
    attachment: &mut SessionAttachmentId,
    routing: &Arc<Mutex<AttachmentRouting>>,
) -> mpsc::Sender<TuiEvent> {
    AttachmentRouting::retire_attachment(routing, *attachment);
    *attachment = attachment.next();
    spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        *attachment,
        Some(routing.clone()),
    )
}
