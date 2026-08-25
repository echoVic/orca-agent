//! Hosted TUI context-mutation transaction ownership.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::RunConfig;
use orca_runtime::history;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};

use crate::hosted_session::announce_runtime_ready;
use crate::hosted_session_lifecycle::ensure_hosted_thread;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::{TuiEvent, TuiMemoryScope};

pub(crate) enum HostedContextAction {
    Remember { scope: TuiMemoryScope, note: String },
    Compact,
    Backtrack,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_context_action(
    action: HostedContextAction,
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) {
    match action {
        HostedContextAction::Remember { scope, note } => {
            let context = format!("[Pinned remembered note]\n{}", note.trim());
            let thread_was_missing = thread.is_none();
            let cfg = config.lock().unwrap().clone();
            if thread.is_none()
                && let Err(error) = ensure_hosted_thread(
                    thread,
                    host,
                    &cfg,
                    preloaded,
                    "Remembered context",
                    event_tx,
                )
            {
                let _ = event_tx.send(TuiEvent::Error(error));
                return;
            }
            if thread_was_missing {
                announce_runtime_ready(
                    thread.as_ref().expect("remember thread"),
                    event_tx,
                    control,
                );
            }
            if let Some(runtime_thread) = thread.as_ref() {
                let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
                let cwd = cfg
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                match actions.remember(scope, &cwd, &note) {
                    Ok(path) => {
                        let _ = event_tx.send(TuiEvent::Notice(format!(
                            "Remembered in {}.",
                            path.display()
                        )));
                        if let Err(error) = actions.add_pinned_context(&context) {
                            let _ = event_tx.send(TuiEvent::Error(format!(
                                "memory was saved but could not be pinned: {error}"
                            )));
                        }
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to remember: {error}")));
                    }
                }
            }
        }
        HostedContextAction::Compact => {
            let Some(runtime_thread) = thread.as_ref() else {
                let _ = event_tx.send(TuiEvent::Error("nothing to compact".to_string()));
                return;
            };
            let actions = TuiSurfaceActions::new(runtime_thread.typed_surface());
            if let Err(error) = actions.manual_compact(control, event_tx) {
                let _ = event_tx.send(TuiEvent::OperationRejected(format!(
                    "manual compaction failed: {error}"
                )));
            }
        }
        HostedContextAction::Backtrack => {
            let result = thread
                .as_ref()
                .map(|runtime_thread| {
                    TuiSurfaceActions::new(runtime_thread.typed_surface()).backtrack_last_user()
                })
                .transpose();
            match result {
                Ok(Some(Some(prompt))) => {
                    let _ = event_tx.send(TuiEvent::Backtracked { prompt });
                }
                Ok(Some(None)) | Ok(None) => {
                    let _ = event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(error.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::HistoryMode;

    #[test]
    fn empty_context_actions_preserve_state_and_shape_errors() {
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let initial_model = config.lock().unwrap().model.display_name().to_string();
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
        let host = runtime.handle();
        let mut thread = None;

        handle_hosted_context_action(
            HostedContextAction::Compact,
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
        );
        handle_hosted_context_action(
            HostedContextAction::Backtrack,
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message)) if message == "nothing to compact"
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message)) if message == "nothing to backtrack"
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(thread.is_none());
        assert_eq!(config.lock().unwrap().model.display_name(), initial_model);
        assert!(preloaded.lock().unwrap().is_none());

        runtime.shutdown().unwrap();
    }

    #[test]
    fn remember_without_thread_announces_ready_before_success_and_commits_memory_and_pin() {
        let home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = HistoryMode::Record;
        run_config.cwd = Some(home.path().to_path_buf());
        let config = Arc::new(Mutex::new(run_config));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
        let host = runtime.handle();
        let mut thread = None;

        handle_hosted_context_action(
            HostedContextAction::Remember {
                scope: TuiMemoryScope::User,
                note: "prefer durable runtime ownership".to_string(),
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
            .expect("runtime ready before remember result");
        let notice_index = events
            .iter()
            .position(|event| {
                matches!(event, TuiEvent::Notice(message)
                    if message.starts_with("Remembered in "))
            })
            .expect("remember success notice");
        assert!(ready_index < notice_index);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TuiEvent::Error(_)))
        );
        assert!(
            orca_runtime::memory::load_for_cwd(home.path())
                .user
                .as_deref()
                .is_some_and(|memory| memory.contains("prefer durable runtime ownership"))
        );

        let runtime_thread = thread.as_ref().expect("thread is initialized");
        let typed_thread = runtime_thread.typed_surface();
        let attachment =
            match typed_thread
                .surface()
                .attach_fresh(orca_runtime::surface::FreshAttachRequest {
                    request_id: orca_runtime::surface::SurfaceRequestId::new(),
                    role: orca_runtime::surface::SurfaceAttachmentRole::Tui,
                    requested_capabilities: std::collections::BTreeSet::from([
                        orca_runtime::surface::SurfaceCapability::ReadSnapshot,
                    ]),
                    interaction_capabilities: std::collections::BTreeSet::new(),
                }) {
                orca_runtime::surface::AttachResult::FreshAttached { attachment } => attachment,
                _ => panic!("typed pinned context attach failed"),
            };
        assert!(
            attachment
                .baseline
                .snapshot
                .pinned_context
                .entries
                .iter()
                .any(|entry| entry.content.as_str().contains("durable runtime ownership"))
        );
        typed_thread.surface().detach(
            &attachment.client,
            orca_runtime::surface::DetachRequest {
                request_id: orca_runtime::surface::SurfaceRequestId::new(),
            },
        );

        thread.unwrap().shutdown().unwrap();
        runtime.shutdown().unwrap();
    }
}
