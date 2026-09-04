//! Hosted TUI action-receive and lifecycle controller ownership.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::attachment_routing::{AttachmentRouting, spawn_attached_event_sender_with_routing};
use crate::background_tasks::{HostedTaskAction, handle_hosted_task_action};
use crate::bridge;
use crate::hosted_child::{
    HostedChildAction, HostedChildFocus, handle_hosted_child_action,
    shutdown_attached_child_on_controller_exit,
};
use crate::hosted_context::{HostedContextAction, handle_hosted_context_action};
use crate::hosted_goal::{HostedGoalAction, handle_hosted_goal_action};
use crate::hosted_operation::{HostedOperationAction, handle_hosted_operation_action};
use crate::hosted_plan::{HostedPlanAction, handle_hosted_plan_action};
use crate::hosted_session::{
    announce_runtime_ready, emit_empty_history_snapshot, emit_typed_history_snapshot,
    typed_history_startup_eligible,
};
use crate::hosted_session_lifecycle::{
    HostedSessionAction, ensure_hosted_thread, handle_hosted_session_action,
};
use crate::hosted_settings::{apply_hosted_settings_action, settings_intent_patches};
use crate::hosted_side::{
    HostedSideAction, HostedSideParent, handle_hosted_side_action, hosted_config_for_active,
    shutdown_attached_side_on_controller_exit,
};
use crate::hosted_submission::{handle_hosted_queued_prompt, handle_hosted_submitted_turn};
use crate::hosted_workflow::{HostedWorkflowAction, handle_hosted_workflow_action};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TaskTranscriptResult, TuiEvent, UserAction};
use crate::slash_command_actions::decode_settings_intent;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_client;
use crate::surface_projection::SurfaceProjectionState;

const IDLE_SURFACE_PROJECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn poll_idle_surface_projection(
    thread: Option<&RuntimeThreadHandle>,
    event_tx: &mpsc::Sender<TuiEvent>,
    last_cursor: &mut Option<orca_runtime::surface::SurfaceCursor>,
) {
    let Some(thread) = thread else {
        *last_cursor = None;
        return;
    };
    let actions = crate::surface_actions::TuiSurfaceActions::new(thread.typed_surface());
    let Ok(snapshot) = actions.read_snapshot() else {
        return;
    };
    if last_cursor.as_ref() == Some(&snapshot.cursor) {
        return;
    }
    *last_cursor = Some(snapshot.cursor.clone());
    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(
        SurfaceProjectionState::from_surface_snapshot(&snapshot),
    )));
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn hosted_tui_controller_loop(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    control: TuiSurfaceTaskControl,
    pending_workflow_notifications: bridge::PendingWorkflowNotifications,
    host: RuntimeHostHandle,
) {
    let root_event_tx = event_tx;
    let mut session_attachment = SessionAttachmentId::new(1);
    let attachment_routing = Arc::new(Mutex::new(AttachmentRouting::new(session_attachment)));
    let mut event_tx = spawn_attached_event_sender_with_routing(
        root_event_tx.clone(),
        session_attachment,
        Some(attachment_routing.clone()),
    );
    AttachmentRouting::switch_attachment(
        &attachment_routing,
        &root_event_tx,
        session_attachment,
        None,
        false,
    );
    let mut thread: Option<RuntimeThreadHandle> = None;
    let mut side_parent: Option<HostedSideParent> = None;
    let mut child_focus: Option<HostedChildFocus> = None;
    let mut last_idle_projection_cursor = None;

    let startup_history_mode = config.lock().unwrap().history_mode.clone();
    if typed_history_startup_eligible(&startup_history_mode, &preloaded) {
        let cfg = config.lock().unwrap().clone();
        let selector = match &startup_history_mode {
            HistoryMode::Resume(selector)
            | HistoryMode::ResumeAt { selector, .. }
            | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => unreachable!(),
        };
        let thread_was_missing = thread.is_none();
        let result = RuntimeSurfaceHostHandle::load_saved_session(selector)
            .map(|transcript| transcript.meta.title)
            .map_err(|error| format!("failed to load saved session metadata: {error}"))
            .and_then(|title| {
                ensure_hosted_thread(&mut thread, &host, &cfg, &preloaded, &title, &event_tx)
            })
            .and_then(|_| {
                emit_typed_history_snapshot(
                    thread.as_ref().expect("startup hosted thread"),
                    &startup_history_mode,
                    None,
                    &event_tx,
                )
            });
        if let Err(error) = result {
            if !cfg.prompt.trim().is_empty() {
                emit_empty_history_snapshot(&event_tx, "Unable to restore saved conversation.");
            }
            if !error.contains("typed TUI snapshot attachment unavailable") {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "failed to restore typed conversation snapshot: {error}"
                )));
            }
        }
        if thread_was_missing && thread.is_some() {
            let runtime_thread = thread.as_ref().expect("startup hosted thread");
            announce_runtime_ready(runtime_thread, &event_tx, &control);
        }
    }

    loop {
        let action: Result<UserAction, ()> = if control.is_shutdown() {
            Ok(UserAction::Cancel)
        } else {
            match action_rx.recv_timeout(IDLE_SURFACE_PROJECTION_POLL_INTERVAL) {
                Ok(action) => Ok(action),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    poll_idle_surface_projection(
                        thread.as_ref(),
                        &event_tx,
                        &mut last_idle_projection_cursor,
                    );
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
            }
        };
        let side_disallowed = (side_parent.is_some() || child_focus.is_some())
            && matches!(
                &action,
                Ok(UserAction::NewSession
                    | UserAction::ForkCurrentSession { .. }
                    | UserAction::RenameCurrentSession { .. }
                    | UserAction::ResumeSavedSession { .. }
                    | UserAction::ForkSavedSession { .. }
                    | UserAction::RenameSavedSession { .. }
                    | UserAction::ArchiveSavedSession { .. }
                    | UserAction::DeleteSavedSession { .. }
                    | UserAction::SetModel(_)
                    | UserAction::Remember { .. }
                    | UserAction::Compact
                    | UserAction::GoalShow
                    | UserAction::GoalSet(_)
                    | UserAction::GoalEdit(_)
                    | UserAction::GoalClear
                    | UserAction::GoalPause
                    | UserAction::GoalResume
                    | UserAction::RunWorkflow { .. }
                    | UserAction::StopTask { .. }
                    | UserAction::ResumeTask { .. }
                    | UserAction::RetryTask { .. }
                    | UserAction::FollowUpTask { .. }
                    | UserAction::Backtrack)
            );
        if side_disallowed {
            let _ = event_tx.send(TuiEvent::OperationRejected(
                "this command is unavailable in a side conversation; return to main first"
                    .to_string(),
            ));
            continue;
        }
        if child_focus.is_some()
            && matches!(
                &action,
                Ok(UserAction::StartSideConversation { .. }
                    | UserAction::ToggleSideConversation
                    | UserAction::CloseSideConversation)
            )
        {
            let _ = event_tx.send(TuiEvent::OperationRejected(
                "side conversations are unavailable while a child is focused; return to parent first"
                    .to_string(),
            ));
            continue;
        }
        match action {
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision,
            }) => {
                handle_hosted_child_action(
                    HostedChildAction::Focus {
                        task_id,
                        expected_revision,
                    },
                    &mut thread,
                    &mut child_focus,
                    side_parent.is_some(),
                    &host,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
                last_idle_projection_cursor = None;
            }
            Ok(UserAction::ReturnToParentThread) => {
                handle_hosted_child_action(
                    HostedChildAction::Return,
                    &mut thread,
                    &mut child_focus,
                    side_parent.is_some(),
                    &host,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
                last_idle_projection_cursor = None;
            }
            Ok(UserAction::StartSideConversation { prompt }) => {
                handle_hosted_side_action(
                    HostedSideAction::Start { prompt },
                    &mut thread,
                    &mut side_parent,
                    &host,
                    &config,
                    &preloaded,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::ToggleSideConversation) => {
                handle_hosted_side_action(
                    HostedSideAction::Toggle,
                    &mut thread,
                    &mut side_parent,
                    &host,
                    &config,
                    &preloaded,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::CloseSideConversation) => {
                handle_hosted_side_action(
                    HostedSideAction::Close,
                    &mut thread,
                    &mut side_parent,
                    &host,
                    &config,
                    &preloaded,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::NewSession) => {
                handle_hosted_session_action(
                    HostedSessionAction::New,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::ForkCurrentSession { title }) => {
                handle_hosted_session_action(
                    HostedSessionAction::ForkCurrent { title },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::RenameCurrentSession { title }) => {
                handle_hosted_session_action(
                    HostedSessionAction::RenameCurrent { title },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::ResumeSavedSession { session_id }) => {
                handle_hosted_session_action(
                    HostedSessionAction::ResumeSaved { session_id },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::ForkSavedSession { session_id }) => {
                handle_hosted_session_action(
                    HostedSessionAction::ForkSaved { session_id },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::RenameSavedSession { session_id, title }) => {
                handle_hosted_session_action(
                    HostedSessionAction::RenameSaved { session_id, title },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::ArchiveSavedSession { session_id }) => {
                handle_hosted_session_action(
                    HostedSessionAction::ArchiveSaved { session_id },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::DeleteSavedSession { session_id }) => {
                handle_hosted_session_action(
                    HostedSessionAction::DeleteSaved { session_id },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &pending_workflow_notifications,
                    &root_event_tx,
                    &mut event_tx,
                    &mut session_attachment,
                    &attachment_routing,
                    &control,
                );
            }
            Ok(UserAction::ImplementApprovedPlan {
                prompt,
                approval_mode,
            }) => {
                handle_hosted_plan_action(
                    HostedPlanAction::ImplementApproved {
                        prompt,
                        approval_mode,
                    },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::Submit(prompt)) => handle_hosted_submitted_turn(
                SubmittedTurn::user(prompt),
                &hosted_config_for_active(side_parent.as_ref(), thread.as_ref(), &config),
                &preloaded,
                &mut thread,
                &event_tx,
                &control,
                &pending_workflow_notifications,
                &host,
            ),
            Ok(UserAction::SubmitWithMentions {
                prompt,
                bindings,
                images,
            }) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::user_with_mentions(prompt, bindings, images),
                    &hosted_config_for_active(side_parent.as_ref(), thread.as_ref(), &config),
                    &preloaded,
                    &mut thread,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                    &host,
                );
            }
            Ok(UserAction::QueuePrompt {
                prompt,
                bindings,
                images,
            }) => {
                handle_hosted_queued_prompt(
                    prompt,
                    bindings,
                    images,
                    &hosted_config_for_active(side_parent.as_ref(), thread.as_ref(), &config),
                    &preloaded,
                    &mut thread,
                    &event_tx,
                    &control,
                    &host,
                );
            }
            Ok(UserAction::PromptQueueControl(action)) => {
                let deleted_id = match &action {
                    orca_runtime::prompt_queue::PromptQueueAction::Delete { id, .. } => {
                        Some(id.clone())
                    }
                    _ => None,
                };
                let result = thread
                    .as_ref()
                    .ok_or_else(|| "prompt queue requires an active session".to_string())
                    .and_then(|thread| {
                        thread
                            .prompt_queue(action)
                            .map_err(|error| error.to_string())
                    });
                match result {
                    Ok(snapshot) => {
                        let _ = event_tx.send(TuiEvent::PromptQueueControlUpdated {
                            deleted_id,
                            snapshot,
                        });
                    }
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::OperationRejected(error));
                    }
                }
            }
            Ok(UserAction::SubmitQueued {
                id,
                prompt,
                bindings,
                images,
            }) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::queued_user_with_mentions(id, prompt, bindings, images),
                    &hosted_config_for_active(side_parent.as_ref(), thread.as_ref(), &config),
                    &preloaded,
                    &mut thread,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                    &host,
                );
            }
            Ok(UserAction::SubmitWorkflowNotification(notification)) => {
                handle_hosted_submitted_turn(
                    SubmittedTurn::workflow_notification(notification),
                    &hosted_config_for_active(side_parent.as_ref(), thread.as_ref(), &config),
                    &preloaded,
                    &mut thread,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                    &host,
                );
            }
            Ok(UserAction::RunWorkflow { name, args }) => {
                handle_hosted_workflow_action(
                    HostedWorkflowAction::Run { name, args },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                );
            }
            Ok(UserAction::Interrupt)
            | Ok(UserAction::BackgroundCurrentTurn)
            | Ok(UserAction::PasteImages { .. }) => {}
            Ok(UserAction::ResumeOperation { operation_id }) => {
                handle_hosted_operation_action(
                    HostedOperationAction::Resume { operation_id },
                    thread.as_ref(),
                    &control,
                    &event_tx,
                );
            }
            Ok(UserAction::CancelOperation { operation_id }) => {
                handle_hosted_operation_action(
                    HostedOperationAction::Cancel { operation_id },
                    thread.as_ref(),
                    &control,
                    &event_tx,
                );
            }
            Ok(UserAction::SetModel(model)) => {
                let patches = decode_settings_intent(&model)
                    .map(settings_intent_patches)
                    .map(Ok)
                    .unwrap_or_else(|| {
                        orca_runtime::surface::NonEmptyText::try_new(model).map(|model| {
                            vec![orca_runtime::surface::RuntimeSettingsPatch::SetModel { model }]
                        })
                    });
                let patches = match patches {
                    Ok(patches) => patches,
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                            "invalid model selection: {error}"
                        )));
                        continue;
                    }
                };
                apply_hosted_settings_action(thread.as_ref(), &config, &event_tx, patches);
            }
            Ok(UserAction::Remember { scope, note }) => {
                handle_hosted_context_action(
                    HostedContextAction::Remember { scope, note },
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                );
            }
            Ok(UserAction::Compact) => {
                handle_hosted_context_action(
                    HostedContextAction::Compact,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                );
            }
            Ok(UserAction::Backtrack) => {
                handle_hosted_context_action(
                    HostedContextAction::Backtrack,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                );
            }
            Ok(UserAction::StopTask { task_id }) => {
                handle_hosted_task_action(
                    HostedTaskAction::Stop { task_id },
                    thread.as_ref(),
                    &control,
                    &event_tx,
                );
            }
            Ok(UserAction::ResumeTask { task_id }) => handle_hosted_task_action(
                HostedTaskAction::Resume { task_id },
                thread.as_ref(),
                &control,
                &event_tx,
            ),
            Ok(UserAction::RetryTask { task_id }) => handle_hosted_task_action(
                HostedTaskAction::Retry { task_id },
                thread.as_ref(),
                &control,
                &event_tx,
            ),
            Ok(UserAction::FollowUpTask { task_id, prompt }) => handle_hosted_task_action(
                HostedTaskAction::FollowUp { task_id, prompt },
                thread.as_ref(),
                &control,
                &event_tx,
            ),
            Ok(UserAction::ForegroundTask { task_id }) => {
                handle_hosted_task_action(
                    HostedTaskAction::Foreground { task_id },
                    thread.as_ref(),
                    &control,
                    &event_tx,
                );
            }
            Ok(UserAction::ReadTaskTranscript(request)) => {
                let result = thread
                    .as_ref()
                    .map(|thread| {
                        surface_client::read_task_transcript(&thread.typed_surface(), &request)
                    })
                    .unwrap_or_else(|| {
                        TaskTranscriptResult::unavailable(
                            "cannot read task transcript before a session exists",
                        )
                    });
                let _ = event_tx.send(TuiEvent::TaskTranscriptResult { request, result });
            }
            Ok(UserAction::ResolveBackgroundApproval { id, approved }) => {
                handle_hosted_task_action(
                    HostedTaskAction::ResolveBackgroundApproval { id, approved },
                    thread.as_ref(),
                    &control,
                    &event_tx,
                );
            }
            Ok(UserAction::GoalShow) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Show,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::GoalSet(objective)) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Set(objective),
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::GoalEdit(objective)) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Edit(objective),
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::GoalClear) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Clear,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::GoalPause) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Pause,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::GoalResume) => {
                handle_hosted_goal_action(
                    HostedGoalAction::Resume,
                    &mut thread,
                    &host,
                    &config,
                    &preloaded,
                    &event_tx,
                    &control,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::RespondToInteraction { .. }) => {}
        }
    }

    if let Some(focus) = child_focus {
        shutdown_attached_child_on_controller_exit(focus);
    } else if let Some(side) = side_parent {
        shutdown_attached_side_on_controller_exit(side);
    } else if let Some(runtime_thread) = thread {
        let _ = runtime_thread.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use orca_core::config::HistoryMode;

    use super::hosted_tui_controller_loop;
    use crate::agent_runtime::TuiAgentRuntime;
    use crate::bridge;
    use crate::operation_controller::TuiSurfaceTaskControl;
    use crate::protocol::{AttachedTuiEvent, TaskTranscriptResult, TuiEvent, UserAction};

    fn next_controller_event(event_rx: &mpsc::Receiver<TuiEvent>) -> TuiEvent {
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("hosted controller event");
            let event = match event {
                TuiEvent::Attached(attached) => {
                    let AttachedTuiEvent { event, .. } = *attached;
                    event
                }
                event => event,
            };
            if !matches!(event, TuiEvent::SessionAttachmentActivated) {
                return event;
            }
        }
    }

    fn spawn_controller() -> (
        crate::test_support::OrcaHomeGuard,
        mpsc::Sender<UserAction>,
        mpsc::Receiver<TuiEvent>,
        TuiAgentRuntime,
    ) {
        let _home = crate::test_support::isolate_orca_home();
        let mut config = crate::test_support::test_run_config();
        config.history_mode = HistoryMode::Disabled;
        let config = Arc::new(Mutex::new(config));
        let preloaded = Arc::new(Mutex::new(None));
        let (action_tx, action_rx) = mpsc::bounded(8);
        let (event_tx, event_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let controller_config = Arc::clone(&config);
        let controller_preloaded = Arc::clone(&preloaded);
        let controller_events = event_tx.clone();
        let pending = bridge::PendingWorkflowNotifications::new();
        let runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            8,
            controller,
            move |controller, commands, host| {
                hosted_tui_controller_loop(
                    controller_config,
                    controller_preloaded,
                    controller_events,
                    commands,
                    controller,
                    pending,
                    host,
                );
            },
        )
        .expect("hosted controller runtime");
        (_home, action_tx, event_rx, runtime)
    }

    #[test]
    fn hosted_controller_dispatches_rejections_in_fifo_order_and_exits_cleanly() {
        let (_home, action_tx, event_rx, mut runtime) = spawn_controller();

        action_tx
            .send(UserAction::ReadTaskTranscript(
                crate::protocol::TaskTranscriptRequest {
                    task_id: "task-transcript".to_string(),
                    expected_revision: 1,
                },
            ))
            .expect("transcript action");
        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::TaskTranscriptResult {
                result: TaskTranscriptResult::Unavailable(error),
                ..
            } if error.message == "cannot read task transcript before a session exists"
        ));

        action_tx
            .send(UserAction::FocusChildThread {
                task_id: "task-child".to_string(),
                expected_revision: 1,
            })
            .expect("focus child action");
        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::OperationRejected(message)
                if message == "focus a main conversation before selecting a child"
        ));

        action_tx
            .send(UserAction::StopTask {
                task_id: "task-stop".to_string(),
            })
            .expect("stop action");
        action_tx
            .send(UserAction::ForegroundTask {
                task_id: "task-foreground".to_string(),
            })
            .expect("foreground action");

        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::Error(message) if message == "cannot stop task before a session exists"
        ));
        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::Error(message)
                if message == "cannot foreground task before a session exists"
        ));

        action_tx.send(UserAction::Cancel).expect("cancel action");
        runtime.shutdown().expect("hosted controller shutdown");
    }

    #[test]
    fn malformed_model_action_is_rejected_without_stopping_controller() {
        let (_home, action_tx, event_rx, mut runtime) = spawn_controller();

        action_tx
            .send(UserAction::SetModel("  \t".to_string()))
            .expect("malformed model action");
        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::OperationRejected(message) if message == "invalid model selection: Empty"
        ));

        action_tx
            .send(UserAction::StopTask {
                task_id: "after-malformed-model".to_string(),
            })
            .expect("follow-up stop action");
        assert!(matches!(
            next_controller_event(&event_rx),
            TuiEvent::Error(message) if message == "cannot stop task before a session exists"
        ));

        action_tx.send(UserAction::Cancel).expect("cancel action");
        runtime.shutdown().expect("hosted controller shutdown");
    }
}
