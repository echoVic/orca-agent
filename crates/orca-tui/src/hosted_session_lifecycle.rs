//! Hosted session start, replacement, Goal recovery, preflight, and reaping ownership.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{
    HostedOperationKind, RuntimeHostHandle, RuntimeThreadHandle, RuntimeThreadStartRequest,
};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::attachment_routing::{AttachmentRouting, rotate_attached_event_sender};
use crate::background_tasks::notify_recovered_background_approvals_for_tui;
use crate::bridge;
use crate::hosted_goal::{goal_continuation_prompt, send_goal_history_error};
use crate::hosted_runtime::emit_hosted_operation_error;
use crate::hosted_session::{announce_runtime_ready, emit_typed_history_snapshot};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::surface_actions::{TuiHostActions, TuiSurfaceActions};
use crate::surface_projection::{SessionProjectionPresentation, SurfaceProjectionState};
use crate::types::{SessionAttachmentId, TuiEvent};

#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_hosted_thread(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &RunConfig,
    _preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    title: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    if thread.is_none() {
        let request = RuntimeThreadStartRequest::new(config.clone(), title);
        #[cfg(test)]
        let request = if let Some(transcript) = _preloaded.lock().unwrap().clone() {
            request.with_preloaded(transcript)
        } else {
            request
        };
        let started = host
            .start_thread_with_request(request)
            .map_err(|error| format!("failed to initialize conversation history: {error}"))?;
        #[cfg(test)]
        {
            *_preloaded.lock().unwrap() = None;
        }
        notify_recovered_background_approvals_for_tui(
            &TuiSurfaceActions::new(started.typed_surface()),
            event_tx,
        );
        *thread = Some(started);
    }
    Ok(())
}

pub(crate) fn start_new_hosted_session(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) -> Result<SurfaceProjectionState, String> {
    ensure_current_session_switchable(thread.as_ref())?;

    let mut next_config = config.lock().unwrap().clone();
    next_config.history_mode = HistoryMode::Record;
    next_config.prompt.clear();
    next_config.show_session_picker = false;
    let request = RuntimeThreadStartRequest::new(next_config.clone(), "New conversation");
    let started = host
        .start_thread_with_request(request)
        .map_err(|error| format!("failed to start a new conversation: {error}"))?;
    let (started, projection) = preflight_started_session(started, "start a new conversation")?;

    install_hosted_session(
        thread,
        started,
        next_config,
        config,
        preloaded,
        pending_workflow_notifications,
    );
    Ok(projection)
}

fn ensure_current_session_switchable(current: Option<&RuntimeThreadHandle>) -> Result<(), String> {
    let Some(current) = current else {
        return Ok(());
    };
    let snapshot = TuiSurfaceActions::new(current.typed_surface())
        .read_snapshot()
        .map_err(|error| format!("failed to inspect the current conversation: {error}"))?;
    let has_non_terminal_task = snapshot.tasks.iter().any(|task| {
        !matches!(
            task.status,
            orca_runtime::surface::SurfaceTaskStatus::Stopped
                | orca_runtime::surface::SurfaceTaskStatus::Completed
                | orca_runtime::surface::SurfaceTaskStatus::Failed
                | orca_runtime::surface::SurfaceTaskStatus::Cancelled
        )
    });
    let has_non_terminal_workflow = snapshot.workflows.iter().any(|workflow| {
        !matches!(
            workflow.status,
            orca_runtime::surface::SurfaceWorkflowStatus::Stopped
                | orca_runtime::surface::SurfaceWorkflowStatus::Completed
                | orca_runtime::surface::SurfaceWorkflowStatus::Failed
                | orca_runtime::surface::SurfaceWorkflowStatus::Cancelled
        )
    });
    let has_active_goal = snapshot
        .goal
        .as_ref()
        .is_some_and(|goal| matches!(goal.state, orca_runtime::surface::SurfaceGoalState::Active));
    if snapshot.foreground_operation.is_some()
        || !snapshot.queued_operations.is_empty()
        || !snapshot.background_operations.is_empty()
        || has_non_terminal_task
        || has_non_terminal_workflow
        || has_active_goal
    {
        return Err("current conversation has active work".to_string());
    }
    Ok(())
}

pub(crate) fn reap_hosted_thread(thread: RuntimeThreadHandle) {
    let fallback = thread.clone();
    let result = std::thread::Builder::new()
        .name("orca-tui-session-reaper".to_string())
        .spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut delay = Duration::from_millis(10);
            loop {
                if thread.shutdown().is_ok() {
                    return;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    eprintln!(
                        "orca: runtime session reaper exhausted its 5-second shutdown budget"
                    );
                    return;
                }
                std::thread::sleep(delay.min(remaining));
                delay = delay.saturating_mul(2).min(Duration::from_millis(250));
            }
        });
    if result.is_err() {
        eprintln!("orca: failed to start runtime session reaper; shutting down inline");
        let _ = fallback.shutdown();
    }
}

pub(crate) fn preflight_started_session(
    started: RuntimeThreadHandle,
    operation: &str,
) -> Result<(RuntimeThreadHandle, SurfaceProjectionState), String> {
    let projection = TuiSurfaceActions::new(started.typed_surface())
        .read_snapshot()
        .map(|snapshot| SurfaceProjectionState::from_surface_snapshot(&snapshot))
        .map_err(|error| format!("failed to project conversation before {operation}: {error}"));
    match projection {
        Ok(projection) if projection.session_id.as_deref() == started.session_id() => {
            Ok((started, projection))
        }
        Ok(_) => {
            reap_hosted_thread(started);
            Err(format!(
                "failed to project conversation before {operation}: snapshot identity did not match the runtime handle"
            ))
        }
        Err(error) => {
            reap_hosted_thread(started);
            Err(error)
        }
    }
}

fn install_hosted_session(
    thread: &mut Option<RuntimeThreadHandle>,
    started: RuntimeThreadHandle,
    next_config: RunConfig,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    let previous = thread.replace(started);
    *config.lock().unwrap() = next_config;
    *preloaded.lock().unwrap() = None;
    pending_workflow_notifications.clear();
    if let Some(previous) = previous {
        reap_hosted_thread(previous);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_forked_hosted_session(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    title: Option<String>,
) -> Result<(HistoryMode, SurfaceProjectionState), String> {
    ensure_current_session_switchable(thread.as_ref())?;
    let source_id = thread
        .as_ref()
        .and_then(RuntimeThreadHandle::session_id)
        .ok_or_else(|| "current conversation is not resumable yet".to_string())?
        .to_string();
    let mode = HistoryMode::Fork(source_id);
    let fork_title = title.unwrap_or_else(|| "Forked conversation".to_string());
    let mut next_config = config.lock().unwrap().clone();
    next_config.history_mode = mode.clone();
    next_config.prompt.clear();
    next_config.show_session_picker = false;
    let request = RuntimeThreadStartRequest::new(next_config.clone(), fork_title.clone());
    let started = host
        .start_thread_with_request(request)
        .map_err(|error| format!("failed to fork conversation: {error}"))?;
    let (started, projection) = preflight_started_session(started, "fork conversation")?;

    install_hosted_session(
        thread,
        started,
        next_config,
        config,
        preloaded,
        pending_workflow_notifications,
    );
    Ok((mode, projection))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn switch_saved_hosted_session(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    mode: HistoryMode,
    title: Option<String>,
) -> Result<(HistoryMode, SurfaceProjectionState), String> {
    ensure_current_session_switchable(thread.as_ref())?;
    let selector = match &mode {
        HistoryMode::Resume(selector)
        | HistoryMode::ResumeAt { selector, .. }
        | HistoryMode::Fork(selector) => selector,
        HistoryMode::Record | HistoryMode::Disabled => {
            return Err("saved-session switch requires resume or fork mode".to_string());
        }
    };
    let transcript = RuntimeSurfaceHostHandle::load_saved_session(selector)
        .map_err(|error| format!("failed to load saved conversation: {error}"))?;
    let switch_title = title.unwrap_or_else(|| match mode {
        HistoryMode::Resume(_) => transcript.meta.title.clone(),
        HistoryMode::ResumeAt { .. } => transcript.meta.title.clone(),
        HistoryMode::Fork(_) => format!("Fork of {}", transcript.meta.title),
        HistoryMode::Record | HistoryMode::Disabled => unreachable!(),
    });
    let mut next_config = config.lock().unwrap().clone();
    next_config.history_mode = mode.clone();
    next_config.prompt.clear();
    next_config.show_session_picker = false;
    let request = RuntimeThreadStartRequest::new(next_config.clone(), switch_title)
        .with_preloaded(transcript);
    let started = host
        .start_thread_with_request(request)
        .map_err(|error| format!("failed to switch saved conversation: {error}"))?;
    let (started, projection) = preflight_started_session(started, "switch conversation")?;

    install_hosted_session(
        thread,
        started,
        next_config,
        config,
        preloaded,
        pending_workflow_notifications,
    );
    Ok((mode, projection))
}

pub(crate) fn refresh_saved_session_picker(event_tx: &mpsc::Sender<TuiEvent>, notice: String) {
    match RuntimeSurfaceHostHandle::list_saved_session_page(
        0,
        crate::session_picker_actions::SESSION_PICKER_PAGE_SIZE,
        None,
    ) {
        Ok(page) => {
            let _ = event_tx.send(TuiEvent::SavedSessionsUpdated {
                sessions: page.sessions,
                next_offset: page.next_offset,
                backfill_complete: page.backfill_complete,
                notice,
            });
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                "saved conversation changed, but the list could not be refreshed: {error}"
            )));
        }
    }
}

pub(crate) enum HostedSessionAction {
    New,
    ForkCurrent { title: Option<String> },
    RenameCurrent { title: String },
    ResumeSaved { session_id: String },
    ForkSaved { session_id: String },
    RenameSaved { session_id: String, title: String },
    ArchiveSaved { session_id: String },
    DeleteSaved { session_id: String },
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_session_action(
    action: HostedSessionAction,
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    root_event_tx: &mpsc::Sender<TuiEvent>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    attachment_routing: &Arc<Mutex<AttachmentRouting>>,
    control: &TuiSurfaceTaskControl,
) {
    match action {
        HostedSessionAction::New => match start_new_hosted_session(
            thread,
            host,
            config,
            preloaded,
            pending_workflow_notifications,
        ) {
            Ok(projection) => {
                rotate_attached_event_sender(
                    root_event_tx,
                    session_attachment,
                    event_tx,
                    Some(attachment_routing),
                );
                let _ = event_tx.send(TuiEvent::SessionProjectionReset(Box::new(projection)));
                announce_runtime_ready(
                    thread.as_ref().expect("new hosted thread"),
                    event_tx,
                    control,
                );
                let _ = event_tx.send(TuiEvent::NewSessionStarted);
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::OperationRejected(error));
            }
        },
        HostedSessionAction::ForkCurrent { title } => match start_forked_hosted_session(
            thread,
            host,
            config,
            preloaded,
            pending_workflow_notifications,
            title,
        ) {
            Ok((mode, projection)) => {
                rotate_attached_event_sender(
                    root_event_tx,
                    session_attachment,
                    event_tx,
                    Some(attachment_routing),
                );
                let _ = event_tx.send(TuiEvent::SessionProjectionReset(Box::new(projection)));
                announce_runtime_ready(
                    thread.as_ref().expect("forked hosted thread"),
                    event_tx,
                    control,
                );
                if let Some(runtime_thread) = thread.as_ref()
                    && let Err(error) = emit_typed_history_snapshot(
                        runtime_thread,
                        &mode,
                        Some(SessionProjectionPresentation::Forked),
                        event_tx,
                    )
                {
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to project forked conversation: {error}"
                    )));
                }
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::OperationRejected(error));
            }
        },
        HostedSessionAction::RenameCurrent { title } => {
            let Some(session_id) = thread
                .as_ref()
                .and_then(RuntimeThreadHandle::session_id)
                .map(str::to_string)
            else {
                let _ = event_tx.send(TuiEvent::OperationRejected(
                    "current conversation is not resumable yet".to_string(),
                ));
                return;
            };
            let rename_result = thread
                .as_ref()
                .map(|runtime_thread| {
                    TuiSurfaceActions::new(runtime_thread.typed_surface())
                        .rename_current_session(&session_id, &title)
                })
                .unwrap_or_else(|| {
                    Err(std::io::Error::other(
                        "current conversation surface is unavailable",
                    ))
                });
            match rename_result {
                Ok(projection) => {
                    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to rename conversation: {error}"
                    )));
                }
            }
        }
        HostedSessionAction::ResumeSaved { session_id } => {
            match switch_saved_hosted_session(
                thread,
                host,
                config,
                preloaded,
                pending_workflow_notifications,
                HistoryMode::Resume(session_id),
                None,
            ) {
                Ok((mode, projection)) => {
                    rotate_attached_event_sender(
                        root_event_tx,
                        session_attachment,
                        event_tx,
                        Some(attachment_routing),
                    );
                    if let Some(runtime_thread) = thread.as_ref() {
                        let _ =
                            event_tx.send(TuiEvent::SessionProjectionReset(Box::new(projection)));
                        announce_runtime_ready(runtime_thread, event_tx, control);
                    }
                    if let Some(runtime_thread) = thread.as_ref()
                        && let Err(error) =
                            emit_typed_history_snapshot(runtime_thread, &mode, None, event_tx)
                    {
                        let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                            "failed to project resumed conversation: {error}"
                        )));
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                }
            }
        }
        HostedSessionAction::ForkSaved { session_id } => {
            match switch_saved_hosted_session(
                thread,
                host,
                config,
                preloaded,
                pending_workflow_notifications,
                HistoryMode::Fork(session_id),
                None,
            ) {
                Ok((mode, projection)) => {
                    rotate_attached_event_sender(
                        root_event_tx,
                        session_attachment,
                        event_tx,
                        Some(attachment_routing),
                    );
                    if let Some(runtime_thread) = thread.as_ref() {
                        let _ =
                            event_tx.send(TuiEvent::SessionProjectionReset(Box::new(projection)));
                        announce_runtime_ready(runtime_thread, event_tx, control);
                        if let Err(error) = emit_typed_history_snapshot(
                            runtime_thread,
                            &mode,
                            Some(SessionProjectionPresentation::Forked),
                            event_tx,
                        ) {
                            let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                                "failed to project forked conversation: {error}"
                            )));
                        }
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                }
            }
        }
        HostedSessionAction::RenameSaved { session_id, title } => {
            match TuiHostActions::rename_saved_session(&session_id, &title) {
                Ok(()) => refresh_saved_session_picker(
                    root_event_tx,
                    format!("Renamed conversation to {title}."),
                ),
                Err(error) => {
                    let _ = root_event_tx.send(TuiEvent::SavedSessionActionFailed(format!(
                        "failed to rename conversation: {error}"
                    )));
                }
            }
        }
        HostedSessionAction::ArchiveSaved { session_id } => {
            if thread
                .as_ref()
                .and_then(RuntimeThreadHandle::session_id)
                .is_some_and(|current| current == session_id)
            {
                let _ = root_event_tx.send(TuiEvent::SavedSessionActionFailed(
                    "cannot archive the current conversation".to_string(),
                ));
                return;
            }
            match TuiHostActions::archive_saved_session(&session_id) {
                Ok(()) => refresh_saved_session_picker(
                    root_event_tx,
                    "Archived saved conversation.".to_string(),
                ),
                Err(error) => {
                    let _ = root_event_tx.send(TuiEvent::SavedSessionActionFailed(format!(
                        "failed to archive conversation: {error}"
                    )));
                }
            }
        }
        HostedSessionAction::DeleteSaved { session_id } => {
            if thread
                .as_ref()
                .and_then(RuntimeThreadHandle::session_id)
                .is_some_and(|current| current == session_id)
            {
                let _ = root_event_tx.send(TuiEvent::SavedSessionActionFailed(
                    "cannot delete the current conversation".to_string(),
                ));
                return;
            }
            match TuiHostActions::delete_saved_session(&session_id) {
                Ok(()) => refresh_saved_session_picker(
                    root_event_tx,
                    "Deleted saved conversation.".to_string(),
                ),
                Err(error) => {
                    let _ = root_event_tx.send(TuiEvent::SavedSessionActionFailed(format!(
                        "failed to delete conversation: {error}"
                    )));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_latest_active_goal_hosted(
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    _pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
        send_goal_history_error(event_tx);
        return;
    }
    let goal = match RuntimeSurfaceHostHandle::latest_active_saved_goal() {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to read goals: {error}")));
            return;
        }
    };
    let transcript = match RuntimeSurfaceHostHandle::load_saved_session(&goal.session_id) {
        Ok(transcript) => transcript,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to load goal session {}: {error}",
                goal.session_id
            )));
            return;
        }
    };
    let mut cfg = config.lock().unwrap().clone();
    cfg.history_mode = HistoryMode::Resume(goal.session_id.clone());
    let request =
        RuntimeThreadStartRequest::new(cfg.clone(), &goal.objective).with_preloaded(transcript);
    let resumed = match host.start_thread_with_request(request) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to initialize resumed goal session: {error}"
            )));
            return;
        }
    };
    let resumed_actions = TuiSurfaceActions::new(resumed.typed_surface());
    let active_goal = match resumed_actions.goal(&goal.session_id) {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::Error(
                "goal disappeared while restoring its session".to_string(),
            ));
            let _ = resumed.shutdown();
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to project goal in restored session: {error}"
            )));
            let _ = resumed.shutdown();
            return;
        }
    };
    if let Some(previous) = thread.take() {
        reap_hosted_thread(previous);
    }
    notify_recovered_background_approvals_for_tui(
        &TuiSurfaceActions::new(resumed.typed_surface()),
        event_tx,
    );
    *thread = Some(resumed);
    *preloaded.lock().unwrap() = None;
    if let Ok(mut shared) = config.lock() {
        shared.history_mode = cfg.history_mode.clone();
    }
    if let Some(runtime_thread) = thread.as_ref() {
        let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
        if let Err(error) = actions.resume_goal_and_run_with_started(
            goal_continuation_prompt(&active_goal.objective, 1),
            control,
            event_tx,
            || {
                let _ = event_tx.send(TuiEvent::Notice(
                    "Resumed latest active goal in a restored session.".to_string(),
                ));
            },
        ) {
            emit_hosted_operation_error(event_tx, error, &HostedOperationKind::GoalRun);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel as mpsc;
    use orca_core::config::HistoryMode;

    use super::*;

    #[test]
    fn no_current_session_is_switchable() {
        assert!(super::ensure_current_session_switchable(None).is_ok());
    }

    #[test]
    fn latest_goal_recovery_rejects_disabled_history_without_starting_thread() {
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Disabled;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let pending = crate::bridge::PendingWorkflowNotifications::new();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        runtime.shutdown().expect("runtime host shutdown");
        let mut thread = None;

        super::resume_latest_active_goal_hosted(
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
            &pending,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message))
                if message == crate::hosted_goal::goal_history_error_message()
        ));
        assert!(thread.is_none());
        assert!(preloaded.lock().expect("preloaded state").is_none());
    }

    #[test]
    fn rename_current_without_thread_uses_focused_session_action_owner() {
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let pending = crate::bridge::PendingWorkflowNotifications::new();
        let (root_event_tx, event_rx) = mpsc::unbounded();
        let mut event_tx = root_event_tx.clone();
        let mut attachment = crate::types::SessionAttachmentId::new(1);
        let initial_attachment = attachment;
        let routing = Arc::new(Mutex::new(
            crate::attachment_routing::AttachmentRouting::new(attachment),
        ));
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        runtime.shutdown().expect("runtime host shutdown");
        let mut thread = None;

        handle_hosted_session_action(
            HostedSessionAction::RenameCurrent {
                title: "renamed".to_string(),
            },
            &mut thread,
            &host,
            &config,
            &preloaded,
            &pending,
            &root_event_tx,
            &mut event_tx,
            &mut attachment,
            &routing,
            &control,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "current conversation is not resumable yet"
        ));
        assert!(thread.is_none());
        assert_eq!(attachment, initial_attachment);
        assert!(matches!(
            config.lock().expect("config").history_mode,
            HistoryMode::Record
        ));
        assert!(preloaded.lock().expect("preloaded state").is_none());
    }

    #[test]
    fn destructive_saved_actions_reject_the_current_recorded_session_on_root() {
        let _home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        run_config.show_session_picker = true;
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        let current = host
            .start_thread(run_config.clone(), "protected conversation")
            .expect("recorded runtime thread");
        let session_id = current
            .session_id()
            .expect("recorded session id")
            .to_string();
        let thread_id = current.thread_id().to_string();
        let transcript = RuntimeSurfaceHostHandle::load_saved_session(&session_id)
            .expect("current transcript is loadable before action");
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(Some(transcript)));
        let pending = crate::bridge::PendingWorkflowNotifications::new();
        let (root_event_tx, root_event_rx) = mpsc::unbounded();
        let (attached_event_tx, attached_event_rx) = mpsc::unbounded();
        let mut event_tx = attached_event_tx;
        let mut attachment = crate::types::SessionAttachmentId::new(7);
        let initial_attachment = attachment;
        let routing = Arc::new(Mutex::new(
            crate::attachment_routing::AttachmentRouting::new(attachment),
        ));
        let mut thread = Some(current);
        let control = TuiSurfaceTaskControl::isolated_for_test();

        for (action, expected) in [
            (
                HostedSessionAction::ArchiveSaved {
                    session_id: session_id.clone(),
                },
                "cannot archive the current conversation",
            ),
            (
                HostedSessionAction::DeleteSaved {
                    session_id: session_id.clone(),
                },
                "cannot delete the current conversation",
            ),
        ] {
            handle_hosted_session_action(
                action,
                &mut thread,
                &host,
                &config,
                &preloaded,
                &pending,
                &root_event_tx,
                &mut event_tx,
                &mut attachment,
                &routing,
                &control,
            );

            assert!(matches!(
                root_event_rx.try_recv(),
                Ok(TuiEvent::SavedSessionActionFailed(message)) if message == expected
            ));
            assert!(root_event_rx.try_recv().is_err());
            assert!(attached_event_rx.try_recv().is_err());
            assert_eq!(
                thread.as_ref().map(RuntimeThreadHandle::thread_id),
                Some(thread_id.as_str())
            );
            assert_eq!(attachment, initial_attachment);
            assert!(matches!(
                config.lock().expect("config").history_mode,
                HistoryMode::Record
            ));
            assert!(config.lock().expect("config").show_session_picker);
            assert_eq!(
                preloaded
                    .lock()
                    .expect("preloaded state")
                    .as_ref()
                    .map(|saved| saved.meta.session_id.as_str()),
                Some(session_id.as_str())
            );
            assert_eq!(
                RuntimeSurfaceHostHandle::load_saved_session(&session_id)
                    .expect("current transcript remains loadable")
                    .meta
                    .session_id,
                session_id
            );
            let saved = RuntimeSurfaceHostHandle::list_saved_sessions(100)
                .expect("saved session list remains readable")
                .into_iter()
                .find(|saved| saved.session_id == session_id)
                .expect("current session remains in the active list");
            assert!(!saved.archived);
        }

        thread
            .take()
            .expect("current thread")
            .shutdown()
            .expect("current thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
