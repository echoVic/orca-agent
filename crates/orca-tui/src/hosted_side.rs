//! Hosted Side lifecycle ownership: start/toggle/close transactions, attached
//! parent state, active config selection, sender rotation, and child-before-
//! parent shutdown.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;

use crate::attachment_routing::{AttachmentRouting, spawn_attached_event_sender_with_routing};
use crate::bridge;
use crate::hosted_session::{
    announce_runtime_ready, project_hosted_thread_attached, read_hosted_projection_batch,
};
use crate::hosted_session_lifecycle::{preflight_started_session, reap_hosted_thread};
use crate::hosted_submission::handle_hosted_submitted_turn;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::{SessionAttachmentId, SideParentStatus, TuiEvent};
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle, RuntimeThreadState};

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

pub(crate) enum HostedSideAction {
    Start { prompt: Option<String> },
    Toggle,
    Close,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_side_action(
    action: HostedSideAction,
    thread: &mut Option<RuntimeThreadHandle>,
    side_parent: &mut Option<HostedSideParent>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    root_event_tx: &mpsc::Sender<TuiEvent>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    attachment_routing: &Arc<Mutex<AttachmentRouting>>,
    control: &TuiSurfaceTaskControl,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    match action {
        HostedSideAction::Start { prompt } => {
            if side_parent.is_some() {
                let _ = event_tx.send(TuiEvent::OperationRejected(
                    "a side conversation is already open".to_string(),
                ));
                return;
            }
            let Some(parent) = thread.take() else {
                let _ = event_tx.send(TuiEvent::OperationRejected(
                    "start the main conversation before opening a side conversation".to_string(),
                ));
                return;
            };
            let parent_state = parent.state().unwrap_or(RuntimeThreadState::Unavailable);
            let parent_status = match parent_state {
                RuntimeThreadState::Running { .. } => SideParentStatus::Running,
                RuntimeThreadState::Idle => SideParentStatus::Idle,
                RuntimeThreadState::Unavailable => SideParentStatus::Closed,
            };
            let parent_title = TuiSurfaceActions::new(parent.typed_surface())
                .read_snapshot()
                .map(|snapshot| snapshot.thread.title.as_str().to_string())
                .unwrap_or_else(|_| "main".to_string());
            let mut side_config = config.lock().unwrap().clone();
            side_config.history_mode = HistoryMode::Disabled;
            side_config.auto_memory = false;
            side_config.approval_mode = ApprovalMode::Plan;
            let started =
                match host.start_side_thread(&parent, side_config.clone(), "Side conversation") {
                    Ok(started) => started,
                    Err(error) => {
                        *thread = Some(parent);
                        let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                            "failed to start side conversation: {error}"
                        )));
                        return;
                    }
                };
            let (started, side_projection) =
                match preflight_started_session(started, "start side conversation") {
                    Ok(result) => result,
                    Err(error) => {
                        *thread = Some(parent);
                        let _ = event_tx.send(TuiEvent::OperationRejected(error));
                        return;
                    }
                };
            let side_messages = match read_hosted_projection_batch(&started) {
                Ok((_, messages)) => messages,
                Err(error) => {
                    reap_hosted_thread(started);
                    *thread = Some(parent);
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to project side conversation: {error}"
                    )));
                    return;
                }
            };
            let parent_event_tx = event_tx.clone();
            let parent_attachment = *session_attachment;
            let side_config = Arc::new(Mutex::new(side_config));
            let side_attachment = parent_attachment.next();
            let side_event_tx = spawn_attached_event_sender_with_routing(
                root_event_tx.clone(),
                side_attachment,
                Some(attachment_routing.clone()),
            );
            *side_parent = Some(HostedSideParent {
                thread: parent,
                event_tx: parent_event_tx,
                attachment: parent_attachment,
                side_thread: started,
                side_event_tx,
                side_attachment,
                side_config,
                parent_title: parent_title.clone(),
            });
            *thread = Some(
                side_parent
                    .as_ref()
                    .expect("side parent state")
                    .side_thread
                    .clone(),
            );
            *session_attachment = side_parent
                .as_ref()
                .expect("side parent state")
                .side_attachment;
            *event_tx = side_parent
                .as_ref()
                .expect("side parent state")
                .side_event_tx
                .clone();
            AttachmentRouting::switch_attachment(
                attachment_routing,
                root_event_tx,
                *session_attachment,
                Some(parent_attachment),
                false,
            );
            if let Err(error) = project_hosted_thread_attached(
                side_projection,
                side_messages,
                *session_attachment,
                root_event_tx,
            ) {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "failed to project side conversation: {error}"
                )));
                return;
            }
            let _ = event_tx.send(TuiEvent::SideConversationChanged {
                active: true,
                available: true,
                parent_thread_id: side_parent
                    .as_ref()
                    .expect("side parent state")
                    .thread
                    .thread_id()
                    .to_string(),
                parent_title,
                parent_status,
            });
            announce_runtime_ready(thread.as_ref().expect("side thread"), event_tx);
            let _ = event_tx.send(TuiEvent::Notice(
                "Side conversation opened. Inherited history is reference-only.".to_string(),
            ));
            if let Some(prompt) = prompt {
                handle_hosted_submitted_turn(
                    SubmittedTurn::user(prompt),
                    &side_parent.as_ref().expect("side parent").side_config,
                    preloaded,
                    thread,
                    event_tx,
                    control,
                    pending_workflow_notifications,
                    host,
                );
            }
        }
        HostedSideAction::Toggle => {
            if let Some(side) = side_parent.as_mut() {
                let side_active = thread
                    .as_ref()
                    .is_some_and(|current| current.thread_id() == side.side_thread.thread_id());
                let (target_thread, target_event_tx, target_attachment, target_batch) =
                    if side_active {
                        let batch = match read_hosted_projection_batch(&side.thread) {
                            Ok(batch) => batch,
                            Err(error) => {
                                let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                                    "failed to switch to the main conversation: {error}"
                                )));
                                return;
                            }
                        };
                        (
                            side.thread.clone(),
                            side.event_tx.clone(),
                            side.attachment,
                            batch,
                        )
                    } else {
                        let batch = match read_hosted_projection_batch(&side.side_thread) {
                            Ok(batch) => batch,
                            Err(error) => {
                                let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                                    "failed to switch to the side conversation: {error}"
                                )));
                                return;
                            }
                        };
                        let side_event_tx = rotate_side_event_sender(
                            root_event_tx,
                            &mut side.side_attachment,
                            attachment_routing,
                        );
                        side.side_event_tx = side_event_tx.clone();
                        (
                            side.side_thread.clone(),
                            side_event_tx,
                            side.side_attachment,
                            batch,
                        )
                    };
                *thread = Some(target_thread.clone());
                *event_tx = target_event_tx;
                *session_attachment = target_attachment;
                if side_active {
                    AttachmentRouting::switch_attachment_deferred(
                        attachment_routing,
                        root_event_tx,
                        *session_attachment,
                        Some(side.attachment),
                    );
                } else {
                    AttachmentRouting::switch_attachment(
                        attachment_routing,
                        root_event_tx,
                        *session_attachment,
                        Some(side.attachment),
                        false,
                    );
                }
                if let Err(error) = project_hosted_thread_attached(
                    target_batch.0,
                    target_batch.1,
                    *session_attachment,
                    root_event_tx,
                ) {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "failed to project switched conversation: {error}"
                    )));
                    return;
                }
                if !side_active
                    && let Err(error) = crate::surface_client::rebind_background_presentations(
                        &target_thread.typed_surface(),
                        control,
                        event_tx.clone(),
                    )
                {
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to reattach side background task presentation: {error}"
                    )));
                }
                if side_active {
                    AttachmentRouting::release_deferred_parent_events(
                        attachment_routing,
                        root_event_tx,
                    );
                    let _ = event_tx.send(TuiEvent::SideConversationChanged {
                        active: false,
                        available: true,
                        parent_thread_id: side.thread.thread_id().to_string(),
                        parent_title: side.parent_title.clone(),
                        parent_status: side_parent_status_for_runtime_thread(&side.thread),
                    });
                    announce_runtime_ready(thread.as_ref().expect("parent thread"), event_tx);
                } else {
                    let parent_status = side_parent_status_for_runtime_thread(&side.thread);
                    let _ = event_tx.send(TuiEvent::SideConversationChanged {
                        active: true,
                        available: true,
                        parent_thread_id: side.thread.thread_id().to_string(),
                        parent_title: side.parent_title.clone(),
                        parent_status,
                    });
                    announce_runtime_ready(thread.as_ref().expect("side thread"), event_tx);
                }
            }
        }
        HostedSideAction::Close => {
            if let Some(side) = side_parent.take() {
                let side_active = thread
                    .as_ref()
                    .is_some_and(|current| current.thread_id() == side.side_thread.thread_id());
                let parent_batch = if side_active {
                    match read_hosted_projection_batch(&side.thread) {
                        Ok(batch) => Some(batch),
                        Err(error) => {
                            let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                                "failed to return to the main conversation: {error}"
                            )));
                            *side_parent = Some(side);
                            return;
                        }
                    }
                } else {
                    None
                };
                let side_thread = side.side_thread;
                if let Err(error) = side_thread.shutdown_with_timeout(Duration::from_secs(5)) {
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to close side conversation: {error}"
                    )));
                    *side_parent = Some(HostedSideParent {
                        side_thread,
                        ..side
                    });
                    return;
                }
                if side_active {
                    *thread = Some(side.thread.clone());
                    *event_tx = side.event_tx.clone();
                    *session_attachment = side.attachment;
                    AttachmentRouting::switch_attachment_deferred(
                        attachment_routing,
                        root_event_tx,
                        *session_attachment,
                        Some(side.attachment),
                    );
                    let (parent_projection, parent_messages) = parent_batch.expect("parent batch");
                    if let Err(error) = project_hosted_thread_attached(
                        parent_projection,
                        parent_messages,
                        *session_attachment,
                        root_event_tx,
                    ) {
                        let _ = event_tx.send(TuiEvent::Error(format!(
                            "failed to project main conversation: {error}"
                        )));
                    }
                    AttachmentRouting::release_deferred_parent_events(
                        attachment_routing,
                        root_event_tx,
                    );
                } else {
                    *thread = Some(side.thread);
                    AttachmentRouting::switch_attachment(
                        attachment_routing,
                        root_event_tx,
                        *session_attachment,
                        None,
                        false,
                    );
                }
                let _ = event_tx.send(TuiEvent::SideConversationChanged {
                    active: false,
                    available: false,
                    parent_thread_id: String::new(),
                    parent_title: String::new(),
                    parent_status: SideParentStatus::Idle,
                });
                announce_runtime_ready(thread.as_ref().expect("parent thread"), event_tx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::HistoryMode;

    #[test]
    fn start_without_parent_uses_focused_side_action_owner() {
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let pending = crate::bridge::PendingWorkflowNotifications::new();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let (root_event_tx, root_event_rx) = mpsc::unbounded();
        let (attached_event_tx, attached_event_rx) = mpsc::unbounded();
        let mut event_tx = attached_event_tx;
        let mut attachment = SessionAttachmentId::new(1);
        let initial_attachment = attachment;
        let routing = Arc::new(Mutex::new(AttachmentRouting::new(attachment)));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        runtime.shutdown().expect("runtime host shutdown");
        let mut thread = None;
        let mut side_parent = None;

        handle_hosted_side_action(
            HostedSideAction::Start {
                prompt: Some("not submitted".to_string()),
            },
            &mut thread,
            &mut side_parent,
            &host,
            &config,
            &preloaded,
            &root_event_tx,
            &mut event_tx,
            &mut attachment,
            &routing,
            &control,
            &pending,
        );

        assert!(matches!(
            attached_event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "start the main conversation before opening a side conversation"
        ));
        assert!(attached_event_rx.try_recv().is_err());
        assert!(root_event_rx.try_recv().is_err());
        assert!(thread.is_none());
        assert!(side_parent.is_none());
        assert_eq!(attachment, initial_attachment);
        assert!(matches!(
            config.lock().expect("config").history_mode,
            HistoryMode::Record
        ));
        assert!(preloaded.lock().expect("preloaded state").is_none());
    }
}
