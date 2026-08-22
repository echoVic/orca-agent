//! Hosted Goal orchestration owner. This module is intentionally stateless;
//! runtime handles, configuration, and event channels remain controller-owned.

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel as mpsc;
use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::runtime_host::{HostedOperationKind, RuntimeHostHandle, RuntimeThreadHandle};
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::bridge;
use crate::goal_materialization::materialize_goal_draft;
use crate::hosted_runtime::{
    TuiHostedOperationOutcome, emit_hosted_operation_error, hosted_turn_request,
    run_hosted_ordinary_turn, send_submission_error_with_images,
};
use crate::hosted_session::announce_runtime_ready;
use crate::hosted_session_lifecycle::{ensure_hosted_thread, resume_latest_active_goal_hosted};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::{GoalDraft, TuiEvent};

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
    let rejection_bindings = submitted_turn.rejection_bindings();
    let rejection_images = submitted_turn.rejection_images();
    let queued_id = submitted_turn.queued_id();
    let Some(session_id) = thread.session_id().map(str::to_string) else {
        if queued_id.is_some() {
            send_submission_error_with_images(
                event_tx,
                queued_id,
                rejection_prompt.as_deref(),
                rejection_bindings,
                rejection_images,
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
                send_submission_error_with_images(
                    event_tx,
                    queued_id,
                    rejection_prompt.as_deref(),
                    rejection_bindings,
                    rejection_images,
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
        if let Err(error) = actions.resume_goal_and_run_multimodal(
            submitted_turn.prompt().to_string(),
            submitted_turn.images().to_vec(),
            control,
            event_tx,
        ) {
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

pub(crate) enum HostedGoalAction {
    Show,
    Set(GoalDraft),
    Edit(GoalDraft),
    Clear,
    Pause,
    Resume,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_goal_action(
    action: HostedGoalAction,
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    match action {
        HostedGoalAction::Show => show_hosted_goal(thread, preloaded, config, event_tx),
        HostedGoalAction::Set(draft) => {
            let materialized = match materialize_goal_draft(draft) {
                Ok(materialized) => materialized,
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                        "failed to prepare goal: {error}"
                    )));
                    return;
                }
            };
            let objective = materialized.objective().to_string();
            let cfg = config.lock().unwrap().clone();
            let thread_was_missing = thread.is_none();
            if let Err(error) =
                ensure_hosted_thread(thread, host, &cfg, preloaded, &objective, event_tx)
            {
                let _ = event_tx.send(TuiEvent::OperationRejected(error));
                return;
            }
            if thread_was_missing {
                announce_runtime_ready(thread.as_ref().expect("goal thread"), event_tx);
            }
            let actions = TuiSurfaceActions::new(
                thread
                    .as_ref()
                    .expect("goal thread initialized")
                    .typed_surface(),
            );
            let _ = event_tx.send(TuiEvent::Notice(
                "Starting goal. Automatic continuation will keep running while it remains active."
                    .to_string(),
            ));
            let committed = Cell::new(false);
            let result =
                actions.set_goal_and_run_with_committed(objective, control, event_tx, || {
                    committed.set(true)
                });
            if committed.get() {
                materialized.retain();
            }
            if let Err(error) = result {
                emit_hosted_operation_error(event_tx, error, &HostedOperationKind::GoalRun);
            }
        }
        HostedGoalAction::Edit(draft) => {
            let Some(session_id) =
                existing_hosted_goal_session_id(thread.as_ref(), preloaded, config, event_tx)
            else {
                return;
            };
            let materialized = match materialize_goal_draft(draft) {
                Ok(materialized) => materialized,
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "failed to prepare goal edit: {error}"
                    )));
                    return;
                }
            };
            let objective = materialized.objective().to_string();
            if thread.is_none() {
                let cfg = config.lock().unwrap().clone();
                if let Err(error) =
                    ensure_hosted_thread(thread, host, &cfg, preloaded, &objective, event_tx)
                {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                    return;
                }
                announce_runtime_ready(
                    thread.as_ref().expect("restored Goal edit thread"),
                    event_tx,
                );
            }
            let Some(runtime_thread) = thread.as_ref() else {
                return;
            };
            let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
            let committed = Cell::new(false);
            let result =
                actions.edit_goal_with_committed(&session_id, objective, now_timestamp(), || {
                    committed.set(true)
                });
            if committed.get() {
                materialized.retain();
            }
            match result {
                Ok(projection) => {
                    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(format!("failed to edit goal: {error}")));
                }
            }
        }
        HostedGoalAction::Clear => {
            let Some(session_id) =
                existing_hosted_goal_session_id(thread.as_ref(), preloaded, config, event_tx)
            else {
                return;
            };
            if thread.is_none() {
                let cfg = config.lock().unwrap().clone();
                if let Err(error) =
                    ensure_hosted_thread(thread, host, &cfg, preloaded, "clear Goal", event_tx)
                {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                    return;
                }
                announce_runtime_ready(
                    thread.as_ref().expect("restored Goal clear thread"),
                    event_tx,
                );
            }
            let Some(runtime_thread) = thread.as_ref() else {
                return;
            };
            let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
            match actions.clear_goal(&session_id) {
                Ok(projection) => {
                    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
                }
                Err(error) => {
                    let _ =
                        event_tx.send(TuiEvent::Error(format!("failed to clear goal: {error}")));
                }
            }
        }
        HostedGoalAction::Pause => {
            let Some(_session_id) =
                existing_hosted_goal_session_id(thread.as_ref(), preloaded, config, event_tx)
            else {
                return;
            };
            if thread.is_none() {
                let cfg = config.lock().unwrap().clone();
                if let Err(error) =
                    ensure_hosted_thread(thread, host, &cfg, preloaded, "pause Goal", event_tx)
                {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                    return;
                }
                announce_runtime_ready(
                    thread.as_ref().expect("restored Goal pause thread"),
                    event_tx,
                );
            }
            let Some(runtime_thread) = thread.as_ref() else {
                return;
            };
            let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
            match actions.pause_goal() {
                Ok(projection) => {
                    let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
                }
                Err(error) => {
                    let _ =
                        event_tx.send(TuiEvent::Error(format!("failed to pause goal: {error}")));
                }
            }
        }
        HostedGoalAction::Resume => {
            if current_hosted_goal_session_id(thread.as_ref(), preloaded).is_none() {
                resume_latest_active_goal_hosted(
                    thread,
                    host,
                    config,
                    preloaded,
                    event_tx,
                    control,
                    pending_workflow_notifications,
                );
                return;
            }
            let Some(session_id) = current_hosted_goal_session_id(thread.as_ref(), preloaded)
            else {
                return;
            };
            let goal = thread.as_ref().and_then(|runtime_thread| {
                TuiSurfaceActions::new(runtime_thread.typed_surface())
                    .goal(&session_id)
                    .ok()
                    .flatten()
            });
            if let (Some(runtime_thread), Some(goal)) = (thread.as_ref(), goal) {
                let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
                if let Err(error) = actions.resume_goal_and_run(
                    goal_continuation_prompt(&goal.objective, 1),
                    control,
                    event_tx,
                ) {
                    emit_hosted_operation_error(event_tx, error, &HostedOperationKind::GoalRun);
                }
            }
        }
    }
}

fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel as mpsc;

    use super::*;

    #[test]
    fn empty_recorded_goal_show_uses_focused_owner() {
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let pending = crate::bridge::PendingWorkflowNotifications::new();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        runtime.shutdown().expect("runtime host shutdown");
        let mut thread = None;

        handle_hosted_goal_action(
            HostedGoalAction::Show,
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
            Ok(TuiEvent::GoalStatus(None))
        ));
        assert!(thread.is_none());
        assert!(preloaded.lock().expect("preloaded state").is_none());
    }
}
