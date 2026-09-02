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
    send_submission_error_with_images,
};
use crate::hosted_session::{announce_runtime_ready, start_agent_registry_watcher};
use crate::hosted_session_lifecycle::ensure_hosted_thread;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::submitted_turn::SubmittedTurn;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) fn queue_busy_submission(
    runtime_thread: &RuntimeThreadHandle,
    prompt: String,
    images: Vec<orca_core::conversation::ImageInput>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    runtime_thread
        .prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::Add {
            input: orca_runtime::prompt_queue::PromptQueueInput {
                text: prompt,
                mention_bindings: orca_runtime::mentions::MentionBindings::default(),
                images,
            },
        })
        .map(|snapshot| {
            let _ = event_tx.send(TuiEvent::PromptQueueUpdated(snapshot));
        })
        .map_err(|error| format!("failed to queue while the current task is running: {error}"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_queued_prompt(
    prompt: String,
    bindings: orca_runtime::mentions::MentionBindings,
    images: Vec<crate::composer_images::ComposerImageAttachment>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    thread: &mut Option<RuntimeThreadHandle>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    host: &RuntimeHostHandle,
) {
    let cfg = config.lock().unwrap().clone();
    let rejection_prompt = prompt.clone();
    let submitted = SubmittedTurn::user_with_mentions(prompt, bindings, images);
    let rejection_bindings = submitted.rejection_bindings();
    let rejection_images = submitted.rejection_images();
    let thread_was_missing = thread.is_none();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Err(error) =
        ensure_hosted_thread(thread, host, &cfg, preloaded, submitted.prompt(), event_tx)
    {
        let _ = event_tx.send(TuiEvent::SubmissionRejected {
            queued_id: None,
            prompt: rejection_prompt,
            bindings: rejection_bindings,
            images: rejection_images,
            message: error,
        });
        return;
    }
    if thread_was_missing {
        let runtime_thread = thread.as_ref().expect("queued prompt thread");
        announce_runtime_ready(runtime_thread, event_tx, control);
    }
    let runtime_thread = thread.as_ref().expect("queued prompt thread initialized");
    start_agent_registry_watcher(
        host.clone(),
        runtime_thread.thread_id().to_string(),
        event_tx.clone(),
    );
    let roots = cfg
        .runtime_workspace_roots
        .clone()
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![cwd.clone()]);
    let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
    let input = match submitted.prompt_for_model(&actions, &cwd, &roots) {
        Ok(input) => input,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt: rejection_prompt,
                bindings: rejection_bindings,
                images: rejection_images,
                message: error,
            });
            return;
        }
    };
    match runtime_thread.prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::Add {
        input: orca_runtime::prompt_queue::PromptQueueInput {
            text: input.text,
            mention_bindings: orca_runtime::mentions::MentionBindings::default(),
            images: input.images,
        },
    }) {
        Ok(snapshot) => {
            let _ = event_tx.send(TuiEvent::PromptQueueUpdated(snapshot));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::SubmissionRejected {
                queued_id: None,
                prompt: rejection_prompt,
                bindings: rejection_bindings,
                images: rejection_images,
                message: format!("failed to queue follow-up: {error:?}"),
            });
        }
    }
}

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
    let rejection_bindings = submitted_turn.rejection_bindings();
    let rejection_images = submitted_turn.rejection_images();
    let queued_id = submitted_turn.queued_id();
    let cfg = config.lock().unwrap().clone();
    let thread_was_missing = thread.is_none();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let title_seed = submitted_turn.title_seed(submitted_turn.prompt());
    if let Err(error) = ensure_hosted_thread(thread, host, &cfg, preloaded, &title_seed, event_tx) {
        send_submission_error_with_images(
            event_tx,
            queued_id,
            rejection_prompt.as_deref(),
            rejection_bindings,
            rejection_images,
            error,
        );
        return;
    }
    if thread_was_missing {
        let runtime_thread = thread.as_ref().expect("submitted thread");
        announce_runtime_ready(runtime_thread, event_tx, control);
    }
    let runtime_thread = thread.as_ref().expect("hosted thread initialized");
    start_agent_registry_watcher(
        host.clone(),
        runtime_thread.thread_id().to_string(),
        event_tx.clone(),
    );
    let workspace_roots = cfg
        .runtime_workspace_roots
        .clone()
        .filter(|roots| !roots.is_empty())
        .unwrap_or_else(|| vec![cwd.clone()]);
    let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
    let input = match submitted_turn.prompt_for_model(&actions, &cwd, &workspace_roots) {
        Ok(input) => input,
        Err(error) => {
            send_submission_error_with_images(
                event_tx,
                queued_id,
                rejection_prompt.as_deref(),
                rejection_bindings,
                rejection_images,
                error,
            );
            return;
        }
    };
    let submitted_turn = submitted_turn.with_model_input(input);
    if queued_id.is_none() && rejection_prompt.is_some() && runtime_thread.is_busy() {
        // Queue-driven legacy turns do not own a TUI surface activation. The
        // dispatcher may have armed one while routing this ordinary submit;
        // release it before returning so the next real turn can activate.
        control.cancel_surface_activation_if_idle();
        if let Err(error) = queue_busy_submission(
            runtime_thread,
            submitted_turn.prompt().to_string(),
            submitted_turn.images().to_vec(),
            event_tx,
        ) {
            send_submission_error_with_images(
                event_tx,
                None,
                rejection_prompt.as_deref(),
                rejection_bindings,
                rejection_images,
                error,
            );
        }
        return;
    }
    if runtime_thread.session_id().is_none() {
        let request = hosted_turn_request(&submitted_turn, false);
        match run_hosted_ordinary_turn(&cfg, runtime_thread, request, event_tx, control) {
            Ok(_) => {}
            Err(_error)
                if queued_id.is_none()
                    && rejection_prompt.is_some()
                    && runtime_thread.is_busy() =>
            {
                control.cancel_surface_activation_if_idle();
                if let Err(queue_error) = queue_busy_submission(
                    runtime_thread,
                    submitted_turn.prompt().to_string(),
                    submitted_turn.images().to_vec(),
                    event_tx,
                ) {
                    send_submission_error_with_images(
                        event_tx,
                        None,
                        rejection_prompt.as_deref(),
                        rejection_bindings,
                        rejection_images,
                        queue_error,
                    );
                }
            }
            Err(error) => emit_hosted_operation_error(event_tx, error, &HostedOperationKind::Turn),
        }
        return;
    }
    run_hosted_goal_run(
        &cfg,
        runtime_thread,
        submitted_turn,
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
    use crate::protocol::TuiEvent;
    use crate::submitted_turn::SubmittedTurn;

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
                ..
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
        let pending = bridge::PendingWorkflowNotifications::new();
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let mut thread = None;
        let control = TuiSurfaceTaskControl::isolated_for_test();

        super::handle_hosted_submitted_turn(
            SubmittedTurn::queued_user_with_mentions(42, prompt.to_string(), bindings, Vec::new()),
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
                        ..
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

    #[test]
    fn first_queued_prompt_announces_runtime_ready_before_rejection() {
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
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let mut thread = None;
        let control = TuiSurfaceTaskControl::isolated_for_test();

        super::handle_hosted_queued_prompt(
            prompt.to_string(),
            bindings,
            Vec::new(),
            &config,
            &preloaded,
            &mut thread,
            &event_tx,
            &control,
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
                        queued_id: None,
                        prompt,
                        message,
                        ..
                    } if prompt == "review @gone.txt"
                        && message.contains("failed to resolve bound @gone.txt")
                )
            })
            .expect("queued prompt rejection");
        assert!(ready < rejected, "events: {events:?}");
        thread
            .take()
            .expect("started runtime thread")
            .shutdown()
            .expect("runtime thread shutdown");
        host.shutdown().expect("runtime host shutdown");
    }
}
