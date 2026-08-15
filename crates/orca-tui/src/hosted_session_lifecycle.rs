//! Hosted session start, replacement, preflight, and reaping ownership.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{
    RuntimeHostHandle, RuntimeThreadHandle, RuntimeThreadStartRequest,
};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::background_tasks::notify_recovered_background_approvals_for_tui;
use crate::bridge;
use crate::surface_actions::TuiSurfaceActions;
use crate::surface_projection::SurfaceProjectionState;
use crate::types::TuiEvent;

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

#[cfg(test)]
mod tests {
    #[test]
    fn no_current_session_is_switchable() {
        assert!(super::ensure_current_session_switchable(None).is_ok());
    }
}
