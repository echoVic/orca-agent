//! Stateful hosted submitted-turn orchestration.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::RunConfig;
use orca_runtime::history;
use orca_runtime::runtime_host::{HostedOperationKind, RuntimeHostHandle, RuntimeThreadHandle};

use crate::bridge;
use crate::hosted_goal::run_hosted_goal_run;
use crate::hosted_runtime::{
    emit_hosted_operation_error, hosted_turn_request, run_hosted_ordinary_turn,
    send_submission_error,
};
use crate::hosted_session::announce_runtime_ready;
use crate::hosted_session_lifecycle::ensure_hosted_thread;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::TuiEvent;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_submitted_turn(
    submitted_turn: SubmittedTurn,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    thread: &mut Option<RuntimeThreadHandle>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    _pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
    host: &RuntimeHostHandle,
) {
    let rejection_prompt = submitted_turn.rejection_prompt().map(str::to_string);
    let queued_id = submitted_turn.queued_id();
    let cfg = config.lock().unwrap().clone();
    let thread_was_missing = thread.is_none();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let title_seed = submitted_turn.title_seed(submitted_turn.prompt());
    if let Err(error) = ensure_hosted_thread(thread, host, &cfg, preloaded, &title_seed, event_tx) {
        send_submission_error(event_tx, queued_id, rejection_prompt.as_deref(), error);
        return;
    }
    if thread_was_missing {
        announce_runtime_ready(thread.as_ref().expect("submitted thread"), event_tx);
    }
    let runtime_thread = thread.as_ref().expect("hosted thread initialized");
    let workspace_roots = cfg
        .runtime_workspace_roots
        .clone()
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![cwd.clone()]);
    let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
    let prompt = match submitted_turn.prompt_for_model(&actions, &cwd, &workspace_roots) {
        Ok(prompt) => prompt,
        Err(error) => {
            send_submission_error(event_tx, queued_id, rejection_prompt.as_deref(), error);
            return;
        }
    };
    if runtime_thread.session_id().is_none() {
        let request = hosted_turn_request(&submitted_turn.with_model_prompt(prompt), false);
        if let Err(error) =
            run_hosted_ordinary_turn(&cfg, runtime_thread, request, event_tx, control)
        {
            emit_hosted_operation_error(event_tx, error, &HostedOperationKind::Turn);
        }
        return;
    }
    run_hosted_goal_run(
        &cfg,
        runtime_thread,
        submitted_turn.with_model_prompt(prompt),
        orca_core::goal_runtime::GoalTurnOrigin::User,
        event_tx,
        control,
    );
    if cfg.desktop_notifications {
        let _ = orca_runtime::notify::notify("Orca", "Task completed");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel as mpsc;
    use orca_core::config::HistoryMode;
    use orca_runtime::history;

    use crate::bridge;
    use crate::operation_controller::TuiSurfaceTaskControl;
    use crate::submitted_turn::SubmittedTurn;
    use crate::types::TuiEvent;

    fn transcript(session_id: &str) -> history::SessionTranscript {
        history::SessionTranscript {
            meta: history::SessionMeta {
                schema_version: 1,
                session_id: session_id.to_string(),
                cwd: "/tmp".to_string(),
                provider: "mock".to_string(),
                model: Some("auto".to_string()),
                title: "preserved conversation".to_string(),
                created_at: chrono::Utc::now(),
                parent_id: None,
                forked: false,
                approval_mode: None,
                active_permission_profile: None,
                runtime_workspace_roots: Vec::new(),
                permission_rules: Default::default(),
                additional_working_directories: Vec::new(),
                metadata_writable_directories: Vec::new(),
                network_domain_permissions: Default::default(),
            },
            messages: Vec::new(),
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            completion_status: None,
            completion_error: None,
            next_event_seq: 0,
            semantic_events: Vec::new(),
            path: std::env::temp_dir().join("hosted-submission-preserved.jsonl"),
        }
    }

    #[test]
    fn startup_failure_rejects_original_prompt_and_preserves_preload() {
        let _home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(Some(transcript("preserved-session"))));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let pending = bridge::PendingWorkflowNotifications::new();
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host_handle = host.handle();
        host.shutdown().expect("runtime host shutdown");
        let mut thread = None;

        super::handle_hosted_submitted_turn(
            SubmittedTurn::user("retry me".to_string()),
            &config,
            &preloaded,
            &mut thread,
            &event_tx,
            &control,
            &pending,
            &host_handle,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt,
                message,
            }) if prompt == "retry me"
                && message.contains("failed to initialize conversation history")
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(thread.is_none());
        assert_eq!(
            preloaded
                .lock()
                .expect("preloaded transcript")
                .as_ref()
                .map(|transcript| transcript.meta.session_id.as_str()),
            Some("preserved-session")
        );
    }

    #[test]
    fn queued_mention_failure_preserves_identity_after_runtime_ready() {
        let _home = crate::test_support::isolate_orca_home();
        let root = tempfile::tempdir().expect("workspace root");
        let root_path = root
            .path()
            .canonicalize()
            .expect("canonical workspace root");
        let mut run_config = crate::test_support::test_run_config();
        run_config.cwd = Some(root_path.clone());
        run_config.runtime_workspace_roots = Some(vec![root_path.clone()]);
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let prompt = "review @gone.txt";
        let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
            prompt,
            vec![orca_runtime::mentions::MentionBinding {
                start: 7,
                end: prompt.len(),
                visible: "@gone.txt".to_string(),
                target: orca_runtime::mentions::MentionTarget::File {
                    root: root_path,
                    path: "gone.txt".to_string(),
                    kind: orca_runtime::mentions::MentionFileKind::File,
                },
            }],
        );
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let pending = bridge::PendingWorkflowNotifications::new();
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let mut thread = None;

        super::handle_hosted_submitted_turn(
            SubmittedTurn::queued_user_with_mentions(42, prompt.to_string(), bindings),
            &config,
            &preloaded,
            &mut thread,
            &event_tx,
            &control,
            &pending,
            &host.handle(),
        );

        let events = event_rx.try_iter().collect::<Vec<_>>();
        let ready = events
            .iter()
            .position(|event| matches!(event, TuiEvent::MentionRuntimeReady(_)))
            .expect("runtime ready event");
        let rejected = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    TuiEvent::SubmissionRejected {
                        queued_id: Some(42),
                        prompt,
                        message,
                    } if prompt == "review @gone.txt"
                        && message.contains("failed to resolve bound @gone.txt")
                )
            })
            .expect("queued rejection");
        assert!(ready < rejected, "events: {events:?}");
        assert!(!events.iter().any(|event| matches!(
            event,
            TuiEvent::QueuedSubmissionStarted { .. } | TuiEvent::SessionCompleted { .. }
        )));
        thread
            .take()
            .expect("started runtime thread")
            .shutdown()
            .expect("runtime thread shutdown");
        host.shutdown().expect("runtime host shutdown");
    }
}
