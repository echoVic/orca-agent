use std::collections::VecDeque;
use std::io;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::channels::{TUI_EVENT_CAPACITY, USER_ACTION_CAPACITY};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{TuiEvent, TuiInteractionKey, UserAction};

// One runtime-event batch, one already-full action mailbox, and one direct
// interaction response can be produced before the frame loop drains acks.
const INTERACTION_ACK_CAPACITY: usize = TUI_EVENT_CAPACITY + USER_ACTION_CAPACITY + 1;
const INTERACTION_ACK_OVERFLOW: &str =
    "TUI interaction acknowledgement queue is full; response result discarded";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InteractionResponseAck {
    Committed {
        key: TuiInteractionKey,
    },
    NoLongerPending {
        key: TuiInteractionKey,
        message: String,
    },
    Failed {
        key: TuiInteractionKey,
        message: String,
    },
}

pub(crate) struct TuiActionDispatcher {
    shutdown_tx: Sender<()>,
    handle: Option<JoinHandle<()>>,
    interaction_ack_rx: Receiver<InteractionResponseAck>,
}

impl TuiActionDispatcher {
    pub(crate) fn spawn(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        controller: TuiSurfaceTaskControl,
        command_capacity: usize,
        backlog_capacity: usize,
    ) -> io::Result<(Self, Receiver<UserAction>)> {
        if command_capacity == 0 || backlog_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TUI dispatcher capacities must be greater than zero",
            ));
        }
        let (command_tx, command_rx) = crossbeam_channel::bounded(command_capacity);
        let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded(1);
        let (interaction_ack_tx, interaction_ack_rx) =
            crossbeam_channel::bounded(INTERACTION_ACK_CAPACITY);
        let handle = thread::Builder::new()
            .name("orca-tui-action-dispatcher".to_string())
            .spawn(move || {
                run_dispatcher(
                    action_rx,
                    event_tx,
                    command_tx,
                    interaction_ack_tx,
                    shutdown_rx,
                    controller,
                    backlog_capacity,
                )
            })?;
        Ok((
            Self {
                shutdown_tx,
                handle: Some(handle),
                interaction_ack_rx,
            },
            command_rx,
        ))
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let _ = self.shutdown_tx.try_send(());
        handle
            .join()
            .map_err(|_| io::Error::other("TUI action dispatcher panicked during shutdown"))
    }

    pub(crate) fn interaction_ack_receiver(&self) -> Receiver<InteractionResponseAck> {
        self.interaction_ack_rx.clone()
    }
}

impl Drop for TuiActionDispatcher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn run_dispatcher(
    action_rx: Receiver<UserAction>,
    event_tx: Sender<TuiEvent>,
    command_tx: Sender<UserAction>,
    interaction_ack_tx: Sender<InteractionResponseAck>,
    shutdown_rx: Receiver<()>,
    surface_control: TuiSurfaceTaskControl,
    backlog_capacity: usize,
) {
    // Keep the frozen mutation inventory's historical receiver name while the
    // concrete value is the runtime-surface-only control.
    let controller = surface_control;
    let mut backlog = VecDeque::with_capacity(backlog_capacity);
    'dispatch: loop {
        while let Some(action) = backlog.pop_front() {
            match command_tx.try_send(action) {
                Ok(()) => {}
                Err(TrySendError::Full(action)) => {
                    backlog.push_front(action);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break 'dispatch,
            }
        }

        if backlog.is_empty() {
            crossbeam_channel::select! {
                recv(shutdown_rx) -> _ => break,
                recv(action_rx) -> action => {
                    let Ok(action) = action else { break };
                    if !route_action(
                        action,
                        &command_tx,
                        &event_tx,
                        &interaction_ack_tx,
                        &controller,
                        &mut backlog,
                        backlog_capacity,
                    ) {
                        break;
                    }
                }
            }
        } else {
            crossbeam_channel::select! {
                recv(shutdown_rx) -> _ => break,
                recv(action_rx) -> action => {
                    let Ok(action) = action else { break };
                    if !route_action(
                        action,
                        &command_tx,
                        &event_tx,
                        &interaction_ack_tx,
                        &controller,
                        &mut backlog,
                        backlog_capacity,
                    ) {
                        break;
                    }
                }
                default(Duration::from_millis(2)) => {}
            }
        }
    }
    controller.shutdown();
}

fn route_action(
    action: UserAction,
    command_tx: &Sender<UserAction>,
    event_tx: &Sender<TuiEvent>,
    interaction_ack_tx: &Sender<InteractionResponseAck>,
    surface_control: &TuiSurfaceTaskControl,
    backlog: &mut VecDeque<UserAction>,
    backlog_capacity: usize,
) -> bool {
    // See `run_dispatcher`: this alias is typed surface presentation state, not
    // the legacy operation controller.
    let controller = surface_control;
    match action {
        UserAction::RespondToInteraction { key, response } => {
            // The frozen mutation inventory retains the historical `broker`
            // family name. This alias is the typed surface control itself; it
            // owns no waiter or response state.
            let broker = surface_control;
            let result = broker.respond(&key, &response);
            let ack = interaction_response_ack(key, result);
            match interaction_ack_tx.try_send(ack) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let _ =
                        event_tx.try_send(TuiEvent::Error(INTERACTION_ACK_OVERFLOW.to_string()));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        UserAction::Interrupt => {
            if let Err(error) = controller.interrupt_current() {
                let _ = event_tx.try_send(TuiEvent::OperationRejected(error.to_string()));
            }
        }
        UserAction::BackgroundCurrentTurn => {
            controller.request_background_current();
        }
        UserAction::PasteImages {
            request_id,
            request,
        } => {
            dispatch_image_paste(request_id, request, event_tx);
        }
        UserAction::QueuePrompt {
            prompt,
            bindings,
            images,
        } => {
            match controller.queue_prompt(
                prompt.clone(),
                bindings.clone(),
                crate::composer_images::ComposerImageState::image_inputs(&images),
            ) {
                Ok(Some(snapshot)) => {
                    let _ = event_tx.try_send(TuiEvent::PromptQueueUpdated(snapshot));
                }
                Ok(None) => match enqueue_action(
                    UserAction::QueuePrompt {
                        prompt,
                        bindings,
                        images,
                    },
                    command_tx,
                    backlog,
                    backlog_capacity,
                ) {
                    EnqueueResult::Queued => {}
                    EnqueueResult::Disconnected => return false,
                    EnqueueResult::Overflow(action) => reject_overflowed_action(event_tx, action),
                },
                Err(error) => {
                    let _ = event_tx.try_send(TuiEvent::SubmissionRejected {
                        queued_id: None,
                        prompt,
                        bindings,
                        images,
                        message: error.to_string(),
                    });
                }
            }
        }
        UserAction::PromptQueueControl(action) => {
            let deleted_id = match &action {
                orca_runtime::prompt_queue::PromptQueueAction::Delete { id, .. } => {
                    Some(id.clone())
                }
                _ => None,
            };
            match controller.prompt_queue_action(action.clone()) {
                Ok(Some(snapshot)) => {
                    let _ = event_tx.try_send(TuiEvent::PromptQueueControlUpdated {
                        deleted_id,
                        snapshot,
                    });
                }
                Ok(None) => match enqueue_action(
                    UserAction::PromptQueueControl(action),
                    command_tx,
                    backlog,
                    backlog_capacity,
                ) {
                    EnqueueResult::Queued => {}
                    EnqueueResult::Disconnected => {
                        let _ = event_tx.try_send(TuiEvent::OperationRejected(
                            "TUI command queue is disconnected; queue control rejected".to_string(),
                        ));
                        return false;
                    }
                    EnqueueResult::Overflow(action) => reject_overflowed_action(event_tx, action),
                },
                Err(error) => {
                    let _ = event_tx.try_send(TuiEvent::OperationRejected(error.to_string()));
                }
            }
        }
        UserAction::GoalPause => match controller.pause_current_goal() {
            Ok(true) => {}
            Ok(false) => {
                match enqueue_action(UserAction::GoalPause, command_tx, backlog, backlog_capacity) {
                    EnqueueResult::Queued => {}
                    EnqueueResult::Disconnected => return false,
                    EnqueueResult::Overflow(action) => reject_overflowed_action(event_tx, action),
                }
            }
            Err(error) => {
                let _ = event_tx.try_send(TuiEvent::OperationRejected(error.to_string()));
            }
        },
        UserAction::Cancel => return false,
        action => {
            let arms_surface_activation = matches!(
                &action,
                UserAction::Submit(_)
                    | UserAction::SubmitWithMentions { .. }
                    | UserAction::ImplementApprovedPlan { .. }
                    | UserAction::SubmitWorkflowNotification(_)
                    | UserAction::Compact
                    | UserAction::ResumeOperation { .. }
                    | UserAction::GoalSet(_)
                    | UserAction::GoalResume
                    | UserAction::ResolveBackgroundApproval { .. }
            );
            let armed_here = if arms_surface_activation {
                controller.begin_surface_activation().unwrap_or(false)
            } else {
                false
            };
            match enqueue_action(action, command_tx, backlog, backlog_capacity) {
                EnqueueResult::Queued => {}
                EnqueueResult::Disconnected => return false,
                EnqueueResult::Overflow(action) => {
                    if armed_here {
                        controller.cancel_surface_activation();
                    }
                    reject_overflowed_action(event_tx, action);
                }
            }
        }
    }
    true
}

fn interaction_response_ack(
    key: TuiInteractionKey,
    result: io::Result<bool>,
) -> InteractionResponseAck {
    match result {
        Ok(true) => InteractionResponseAck::Committed { key },
        Ok(false) => InteractionResponseAck::NoLongerPending {
            key,
            message: "runtime-owned interaction is no longer pending".to_string(),
        },
        Err(error) => InteractionResponseAck::Failed {
            key,
            message: error.to_string(),
        },
    }
}

enum EnqueueResult {
    Queued,
    Disconnected,
    Overflow(UserAction),
}

fn enqueue_action(
    action: UserAction,
    command_tx: &Sender<UserAction>,
    backlog: &mut VecDeque<UserAction>,
    backlog_capacity: usize,
) -> EnqueueResult {
    if backlog.is_empty() {
        match command_tx.try_send(action) {
            Ok(()) => return EnqueueResult::Queued,
            Err(TrySendError::Full(action)) => backlog.push_back(action),
            Err(TrySendError::Disconnected(_)) => return EnqueueResult::Disconnected,
        }
    } else if backlog.len() < backlog_capacity {
        backlog.push_back(action);
    } else {
        return EnqueueResult::Overflow(action);
    }
    EnqueueResult::Queued
}

fn reject_overflowed_action(event_tx: &Sender<TuiEvent>, action: UserAction) {
    let message = "TUI command queue is full; command rejected".to_string();
    match action {
        UserAction::Submit(prompt) => {
            let _ = event_tx.try_send(TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt,
                bindings: orca_runtime::mentions::MentionBindings::default(),
                images: Vec::new(),
                message,
            });
        }
        UserAction::SubmitWithMentions {
            prompt,
            bindings,
            images,
        }
        | UserAction::QueuePrompt {
            prompt,
            bindings,
            images,
        } => {
            let _ = event_tx.try_send(TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt,
                bindings,
                images,
                message,
            });
        }
        UserAction::PromptQueueControl(_) | UserAction::PasteImages { .. } => {
            let _ = event_tx.try_send(TuiEvent::OperationRejected(message));
        }
        UserAction::SubmitQueued {
            id,
            prompt,
            bindings,
            images,
        } => {
            let _ = event_tx.try_send(TuiEvent::SubmissionRejected {
                queued_id: Some(id),
                prompt,
                bindings,
                images,
                message,
            });
        }
        UserAction::ImplementApprovedPlan { .. } => {
            let _ = event_tx.try_send(TuiEvent::OperationRejected(message));
        }
        UserAction::NewSession
        | UserAction::ForkCurrentSession { .. }
        | UserAction::RenameCurrentSession { .. }
        | UserAction::ResumeSavedSession { .. }
        | UserAction::ForkSavedSession { .. }
        | UserAction::RenameSavedSession { .. }
        | UserAction::ArchiveSavedSession { .. }
        | UserAction::DeleteSavedSession { .. }
        | UserAction::SubmitWorkflowNotification(_)
        | UserAction::RunWorkflow { .. }
        | UserAction::Compact
        | UserAction::GoalShow
        | UserAction::GoalSet(_)
        | UserAction::GoalEdit(_)
        | UserAction::GoalClear
        | UserAction::GoalPause
        | UserAction::GoalResume => {
            let _ = event_tx.try_send(TuiEvent::OperationRejected(message));
        }
        _ => {
            let _ = event_tx.try_send(TuiEvent::Error(message));
        }
    }
}

fn dispatch_image_paste(
    request_id: u64,
    request: crate::clipboard_image::ImagePasteRequest,
    event_tx: &Sender<TuiEvent>,
) {
    let worker_event_tx = event_tx.clone();
    let spawn = thread::Builder::new()
        .name("orca-clipboard-image".to_string())
        .spawn(move || {
            let result = crate::clipboard_image::read_image_request(request)
                .map_err(|error| error.to_string());
            let _ =
                worker_event_tx.send(TuiEvent::ClipboardImagePasteCompleted { request_id, result });
        });
    if let Err(error) = spawn {
        let _ = event_tx.try_send(TuiEvent::ClipboardImagePasteCompleted {
            request_id,
            result: Err(format!("failed to start clipboard image reader: {error}")),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::io::Cursor;
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use image::ImageEncoder as _;
    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::HistoryMode;
    use orca_runtime::runtime_host::RuntimeHost;
    use orca_runtime::surface::{
        AttachResult, DetachRequest, FreshAttachRequest, SurfaceAttachmentRole, SurfaceCapability,
        SurfaceOperationId, SurfaceRequestId,
    };

    use super::{InteractionResponseAck, TuiActionDispatcher, interaction_response_ack};
    use crate::operation_controller::TuiSurfaceTaskControl;
    use crate::protocol::{
        TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse, UserAction,
    };

    #[test]
    fn image_paste_io_runs_off_the_command_mailbox_and_returns_typed_payload() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("clipboard.png");
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut Cursor::new(&mut png))
            .write_image(&[0, 0xff, 0, 0xff], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, png).unwrap();

        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).unwrap();
        raw_tx
            .send(UserAction::PasteImages {
                request_id: 7,
                request: crate::clipboard_image::ImagePasteRequest::Paths(vec![path]),
            })
            .unwrap();
        raw_tx
            .send(UserAction::Submit("still responsive".to_string()))
            .unwrap();

        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "still responsive"
        ));

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(2)),
            Ok(TuiEvent::ClipboardImagePasteCompleted {
                request_id: 7,
                result: Ok(images),
            }) if images.len() == 1
                && images[0].media_type == "image/png"
                && (images[0].width, images[0].height) == (1, 1)
        ));
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn unknown_interaction_response_does_not_enter_full_command_mailbox() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let key = TuiInteractionKey::new(
            OperationIdAllocator::default().allocate(),
            "ask",
            TuiInteractionKind::UserInput,
        );
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");
        let interaction_ack_rx = dispatcher.interaction_ack_receiver();

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .expect("queue first command");
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .expect("queue second command");
        raw_tx
            .send(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::UserInput("answer".to_string()),
            })
            .expect("queue interaction response");

        assert!(matches!(
            interaction_ack_rx.recv_timeout(Duration::from_secs(1)),
            Ok(InteractionResponseAck::NoLongerPending { message, .. })
                if message.contains("runtime-owned interaction")
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "first"
        ));
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "second"
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn interaction_response_failure_preserves_key_and_exact_error() {
        let key = TuiInteractionKey::new(
            OperationIdAllocator::default().allocate(),
            "retry",
            TuiInteractionKind::McpElicitation,
        );

        assert_eq!(
            interaction_response_ack(key.clone(), Err(io::Error::other("runtime unavailable")),),
            InteractionResponseAck::Failed {
                key,
                message: "runtime unavailable".to_string(),
            }
        );
    }

    #[test]
    fn undrained_ack_lane_retains_each_interaction_response() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let first_key = TuiInteractionKey::new(
            OperationIdAllocator::default().allocate(),
            "first",
            TuiInteractionKind::UserInput,
        );
        let second_key = TuiInteractionKey::new(
            OperationIdAllocator::default().allocate(),
            "second",
            TuiInteractionKind::UserInput,
        );
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");
        let interaction_ack_rx = dispatcher.interaction_ack_receiver();

        for key in [first_key.clone(), second_key.clone()] {
            raw_tx
                .send(UserAction::RespondToInteraction {
                    key,
                    response: TuiInteractionResponse::UserInput("answer".to_string()),
                })
                .expect("queue interaction response");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !raw_tx.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            raw_tx.is_empty(),
            "dispatcher did not consume both responses"
        );

        assert!(matches!(
            interaction_ack_rx.recv_timeout(Duration::from_secs(1)),
            Ok(InteractionResponseAck::NoLongerPending { key, .. }) if key == first_key
        ));
        assert!(matches!(
            interaction_ack_rx.recv_timeout(Duration::from_secs(1)),
            Ok(InteractionResponseAck::NoLongerPending { key, .. }) if key == second_key
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn undrained_acknowledgements_do_not_block_interrupt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        control
            .begin_surface_activation()
            .expect("arm typed surface activation");
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control.clone(), 1, 1)
                .expect("spawn dispatcher");
        let _interaction_ack_rx = dispatcher.interaction_ack_receiver();

        for id in ["first", "second"] {
            raw_tx
                .send(UserAction::RespondToInteraction {
                    key: TuiInteractionKey::new(
                        OperationIdAllocator::default().allocate(),
                        id,
                        TuiInteractionKind::UserInput,
                    ),
                    response: TuiInteractionResponse::UserInput("answer".to_string()),
                })
                .expect("queue interaction response");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !raw_tx.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            raw_tx.is_empty(),
            "dispatcher did not consume both responses"
        );

        raw_tx.send(UserAction::Interrupt).expect("queue interrupt");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !control.has_pending_interrupt() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(control.has_pending_interrupt());
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn acknowledgement_lane_caps_illegal_bursts_without_blocking_interrupt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        control
            .begin_surface_activation()
            .expect("arm typed surface activation");
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control.clone(), 1, 1)
                .expect("spawn dispatcher");
        let interaction_ack_rx = dispatcher.interaction_ack_receiver();
        let ack_capacity =
            crate::channels::TUI_EVENT_CAPACITY + crate::channels::USER_ACTION_CAPACITY + 1;

        for index in 0..=ack_capacity {
            raw_tx
                .send(UserAction::RespondToInteraction {
                    key: TuiInteractionKey::new(
                        OperationIdAllocator::default().allocate(),
                        format!("burst-{index}"),
                        TuiInteractionKind::UserInput,
                    ),
                    response: TuiInteractionResponse::UserInput("answer".to_string()),
                })
                .expect("queue interaction response");
        }
        raw_tx.send(UserAction::Interrupt).expect("queue interrupt");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while (!raw_tx.is_empty() || !control.has_pending_interrupt())
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }

        assert_eq!(interaction_ack_rx.len(), ack_capacity);
        assert!(control.has_pending_interrupt());
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::Error(message))
                if message == "TUI interaction acknowledgement queue is full; response result discarded"
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn shutdown_does_not_wait_for_undrained_acknowledgements() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");
        let _interaction_ack_rx = dispatcher.interaction_ack_receiver();

        for id in ["first", "second"] {
            raw_tx
                .send(UserAction::RespondToInteraction {
                    key: TuiInteractionKey::new(
                        OperationIdAllocator::default().allocate(),
                        id,
                        TuiInteractionKind::UserInput,
                    ),
                    response: TuiInteractionResponse::UserInput("answer".to_string()),
                })
                .expect("queue interaction response");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !raw_tx.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            raw_tx.is_empty(),
            "dispatcher did not consume both responses"
        );

        let (done_tx, done_rx) = mpsc::bounded(1);
        std::thread::spawn(move || {
            let mut dispatcher = dispatcher;
            let _ = done_tx.send(dispatcher.shutdown());
        });
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Ok(()))
        ));
    }

    #[test]
    fn full_command_mailbox_does_not_block_interrupt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        control
            .begin_surface_activation()
            .expect("arm typed surface activation");
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control.clone(), 1, 1)
                .expect("spawn dispatcher");

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .expect("queue first command");
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .expect("queue second command");
        raw_tx.send(UserAction::Interrupt).expect("queue interrupt");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !control.has_pending_interrupt() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(control.has_pending_interrupt());
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn interrupt_reports_surface_cancel_rejection_instead_of_silently_succeeding() {
        let _env = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().expect("temporary ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let host = RuntimeHost::start().expect("runtime host");
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let thread = host
            .start_thread(config, "detached surface cancel")
            .expect("runtime thread");
        let typed_thread = thread.typed_surface();
        let surface = typed_thread.surface();
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            AttachResult::Denied { reason } => {
                panic!("attach typed TUI surface denied: {reason:?}")
            }
            AttachResult::Unavailable { reason } => {
                panic!("attach typed TUI surface unavailable: {reason:?}")
            }
            _ => panic!("attach typed TUI surface returned a non-fresh result"),
        };
        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 7,
        ])
        .expect("surface operation id");
        let control = TuiSurfaceTaskControl::isolated_for_test();
        control
            .begin_surface_activation()
            .expect("arm typed surface activation");
        control
            .install_surface(attachment.client.clone(), operation_id)
            .expect("install typed surface operation");
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );

        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");
        raw_tx.send(UserAction::Interrupt).expect("queue interrupt");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::OperationRejected(message))
                if message.contains("typed surface cancel")
        ));

        dispatcher.shutdown().expect("shutdown dispatcher");
        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn preinstall_interrupt_requires_a_committed_cancel_before_consuming_intent() {
        let _env = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().expect("temporary ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let host = RuntimeHost::start().expect("runtime host");
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let thread = host
            .start_thread(config, "preinstall surface cancel")
            .expect("runtime thread");
        let surface = thread.typed_surface().surface();
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            AttachResult::Denied { reason } => {
                panic!("attach typed TUI surface denied: {reason:?}")
            }
            AttachResult::Unavailable { reason } => {
                panic!("attach typed TUI surface unavailable: {reason:?}")
            }
            _ => panic!("attach typed TUI surface returned a non-fresh result"),
        };
        let _ = surface.detach(
            &attachment.client,
            DetachRequest {
                request_id: SurfaceRequestId::new(),
            },
        );
        let control = TuiSurfaceTaskControl::isolated_for_test();
        control
            .begin_surface_activation()
            .expect("arm typed surface activation");
        assert!(
            control
                .interrupt_current()
                .expect("record preinstall interrupt")
        );
        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 8,
        ])
        .expect("surface operation id");

        let error = control
            .install_surface(attachment.client, operation_id)
            .expect_err("detached cancel cannot commit");
        assert!(error.to_string().contains("typed surface cancel"));
        assert!(control.has_pending_interrupt());
        control.cancel_surface_activation();

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn cancel_shuts_down_surface_control_and_dispatcher_without_command_capacity() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control.clone(), 1, 1)
                .expect("spawn dispatcher");
        raw_tx
            .send(UserAction::Submit("fill".to_string()))
            .expect("fill command mailbox");
        raw_tx.send(UserAction::Cancel).expect("queue cancel");

        dispatcher.shutdown().expect("join dispatcher");
        assert!(control.is_shutdown());
        assert!(matches!(
            control.begin_surface_activation(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn overflowed_submit_is_rejected_with_its_prompt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .unwrap();
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .unwrap();
        raw_tx
            .send(UserAction::Submit("third".to_string()))
            .unwrap();

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::SubmissionRejected {
                prompt, message, ..
            })
                if prompt == "third" && message.contains("queue is full")
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn disconnected_queue_control_reports_operation_rejected() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");
        drop(command_rx);

        raw_tx
            .send(UserAction::PromptQueueControl(
                orca_runtime::prompt_queue::PromptQueueAction::Delete {
                    expected_revision: orca_runtime::prompt_queue::QueueRevision::ZERO,
                    id: orca_runtime::prompt_queue::QueuedSubmissionId::new(),
                },
            ))
            .expect("queue control action");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::OperationRejected(message))
                if message.contains("queue control rejected")
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }
}
