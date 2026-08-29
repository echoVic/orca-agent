//! Hosted TUI saved-workflow transaction ownership.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::RunConfig;
use orca_runtime::history;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};

use crate::hosted_session::announce_runtime_ready;
use crate::hosted_session_lifecycle::ensure_hosted_thread;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) enum HostedWorkflowAction {
    Run { name: String, args: Option<String> },
}

pub(crate) fn handle_hosted_workflow_action(
    action: HostedWorkflowAction,
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) {
    match action {
        HostedWorkflowAction::Run { name, args } => {
            let cfg = config.lock().unwrap().clone();
            let thread_was_missing = thread.is_none();
            if let Err(error) = ensure_hosted_thread(
                thread,
                host,
                &cfg,
                preloaded,
                &format!("Run saved workflow `{name}`"),
                event_tx,
            ) {
                let _ = event_tx.send(TuiEvent::OperationRejected(error));
                return;
            }
            if thread_was_missing {
                announce_runtime_ready(
                    thread.as_ref().expect("workflow thread"),
                    event_tx,
                    control,
                );
            }
            if let Some(runtime_thread) = thread.as_ref() {
                let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
                if let Err(error) = actions.launch_workflow(&name, args.as_deref(), event_tx) {
                    let _ = event_tx.send(TuiEvent::OperationRejected(error));
                    return;
                }
            }
            let _ = event_tx.send(TuiEvent::SessionCompleted {
                status: "success".to_string(),
            });
            if cfg.desktop_notifications {
                let _ = orca_runtime::notify::notify("Orca", "Workflow launched");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::HistoryMode;

    #[test]
    fn empty_workflow_action_announces_ready_then_preserves_thread_on_typed_rejection() {
        let home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Disabled;
        run_config.cwd = Some(home.path().to_path_buf());
        run_config.desktop_notifications = false;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        let mut thread = None;

        handle_hosted_workflow_action(
            HostedWorkflowAction::Run {
                name: "missing".to_string(),
                args: None,
            },
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
        );

        let events: Vec<_> = event_rx.try_iter().collect();
        let ready_index = events
            .iter()
            .position(|event| matches!(event, TuiEvent::MentionRuntimeReady(_)))
            .expect("runtime readiness event");
        let projection_error_index = events
            .iter()
            .position(|event| {
                matches!(event, TuiEvent::Error(message)
                    if message.starts_with("failed to project the active conversation: "))
            })
            .expect("sessionless runtime projection error");
        let rejected_index = events
            .iter()
            .position(|event| {
                matches!(event, TuiEvent::OperationRejected(message)
                    if message == "typed workflow launch requires recorded conversation history")
            })
            .expect("typed workflow rejection");
        assert!(ready_index < projection_error_index);
        assert!(projection_error_index < rejected_index);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TuiEvent::SurfaceProjectionSynced(_)))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TuiEvent::SessionCompleted { .. }))
        );
        assert!(
            thread.is_some(),
            "startup thread remains after launch rejection"
        );

        thread.unwrap().shutdown().expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
