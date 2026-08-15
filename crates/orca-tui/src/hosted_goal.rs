//! Hosted Goal orchestration owner. This module is intentionally stateless;
//! runtime handles, configuration, and event channels remain controller-owned.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{HostedOperationKind, RuntimeThreadHandle};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::hosted_runtime::{
    TuiHostedOperationOutcome, emit_hosted_operation_error, hosted_turn_request,
    run_hosted_ordinary_turn, send_submission_error,
};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::TuiEvent;

pub(crate) fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nWork from current evidence. Preserve the full objective, verify every requirement before completion, and call update_goal only with status \"complete\" when the goal is actually finished or status \"blocked\" after the same blocker has repeated for at least three consecutive goal turns."
    )
}

pub(crate) fn run_hosted_goal_run(
    config: &RunConfig,
    thread: &RuntimeThreadHandle,
    submitted_turn: SubmittedTurn,
    _origin: orca_core::goal_runtime::GoalTurnOrigin,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) {
    let rejection_prompt = submitted_turn.rejection_prompt().map(str::to_string);
    let queued_id = submitted_turn.queued_id();
    let Some(session_id) = thread.session_id().map(str::to_string) else {
        if queued_id.is_some() {
            send_submission_error(
                event_tx,
                queued_id,
                rejection_prompt.as_deref(),
                goal_history_error_message().to_string(),
            );
        } else {
            send_goal_history_error(event_tx);
        }
        return;
    };
    let actions = TuiSurfaceActions::new(thread.typed_surface());
    let active_goal = match actions.goal(&session_id) {
        Ok(goal) => goal.filter(|goal| goal.status.should_continue()),
        Err(error) => {
            if queued_id.is_some() {
                send_submission_error(
                    event_tx,
                    queued_id,
                    rejection_prompt.as_deref(),
                    error.to_string(),
                );
            } else {
                let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            }
            return;
        }
    };
    let _ = _origin;
    if let Some(goal) = active_goal.as_ref() {
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal.clone())));
        if let Some(id) = queued_id {
            let _ = event_tx.send(TuiEvent::QueuedSubmissionStarted { id });
        }
        if let Err(error) =
            actions.resume_goal_and_run(submitted_turn.prompt().to_string(), control, event_tx)
        {
            emit_hosted_operation_error(event_tx, error, &HostedOperationKind::GoalRun);
        }
        return;
    }
    if let Some(id) = queued_id {
        let _ = event_tx.send(TuiEvent::QueuedSubmissionStarted { id });
    }
    let request = hosted_turn_request(&submitted_turn, false);
    let outcome = run_hosted_ordinary_turn(config, thread, request, event_tx, control);
    match outcome {
        Ok(TuiHostedOperationOutcome::Turn { status }) => {
            let _ = status;
        }
        Ok(TuiHostedOperationOutcome::ManualCompaction) => {
            let _ = event_tx.send(TuiEvent::Error(
                "goal run returned a compaction result".to_string(),
            ));
        }
        Err(error) => emit_hosted_operation_error(event_tx, error, &HostedOperationKind::Turn),
    }
}

pub(crate) fn current_hosted_goal_session_id(
    thread: Option<&RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
) -> Option<String> {
    thread
        .and_then(RuntimeThreadHandle::session_id)
        .map(str::to_string)
        .or_else(|| {
            preloaded
                .lock()
                .unwrap()
                .as_ref()
                .map(|transcript| transcript.meta.session_id.clone())
        })
}

pub(crate) fn existing_hosted_goal_session_id(
    thread: Option<&RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<String> {
    if let Some(session_id) = current_hosted_goal_session_id(thread, preloaded) {
        return Some(session_id);
    }
    let history_mode = config.lock().unwrap().history_mode.clone();
    let message = if matches!(history_mode, HistoryMode::Disabled) {
        "persistent goals require recorded history; enable history before using /goal"
    } else {
        "The session must start before you can change a goal."
    };
    let _ = event_tx.send(TuiEvent::Error(message.to_string()));
    None
}

pub(crate) fn show_hosted_goal(
    thread: &Option<RuntimeThreadHandle>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(session_id) = current_hosted_goal_session_id(thread.as_ref(), preloaded) else {
        if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
            send_goal_history_error(event_tx);
        } else {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
        }
        return;
    };
    let result = match thread.as_ref() {
        Some(thread) => TuiSurfaceActions::new(thread.typed_surface()).goal(&session_id),
        None => RuntimeSurfaceHostHandle::project_saved_goal(&session_id)
            .map_err(|error| error.to_string()),
    };
    match result {
        Ok(goal) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(goal));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
        }
    }
}

pub(crate) fn send_goal_history_error(event_tx: &mpsc::Sender<TuiEvent>) {
    let _ = event_tx.send(TuiEvent::Error(goal_history_error_message().to_string()));
}

pub(crate) fn goal_history_error_message() -> &'static str {
    "persistent goals require recorded history; enable history before using /goal"
}
