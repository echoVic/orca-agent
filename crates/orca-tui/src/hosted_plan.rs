//! Hosted TUI approved-plan implementation transaction ownership.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;
use orca_runtime::history;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};

use crate::bridge;
use crate::hosted_settings::{apply_hosted_settings_action, surface_approval_mode};
use crate::hosted_submission::handle_hosted_submitted_turn;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::submitted_turn::SubmittedTurn;

pub(crate) enum HostedPlanAction {
    ImplementApproved {
        prompt: String,
        approval_mode: ApprovalMode,
    },
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hosted_plan_action(
    action: HostedPlanAction,
    thread: &mut Option<RuntimeThreadHandle>,
    host: &RuntimeHostHandle,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    match action {
        HostedPlanAction::ImplementApproved {
            prompt,
            approval_mode,
        } => {
            let settings_applied = apply_hosted_settings_action(
                thread.as_ref(),
                config,
                event_tx,
                vec![
                    orca_runtime::surface::RuntimeSettingsPatch::SetApprovalMode {
                        mode: surface_approval_mode(approval_mode),
                    },
                ],
            );
            if !settings_applied {
                control.cancel_surface_activation();
                return;
            }
            let _ = event_tx.send(TuiEvent::PlanImplementationStarted {
                prompt: prompt.clone(),
            });
            handle_hosted_submitted_turn(
                SubmittedTurn::user(prompt),
                config,
                preloaded,
                thread,
                event_tx,
                control,
                pending_workflow_notifications,
                host,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_plan_implementation_commits_settings_before_submitting_exact_prompt() {
        let home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.cwd = Some(home.path().to_path_buf());
        run_config.history_mode = orca_core::config::HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        let mut thread = None;
        let preloaded = Arc::new(Mutex::new(None));
        let pending_workflow_notifications = bridge::PendingWorkflowNotifications::new();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        assert!(
            control
                .begin_surface_activation()
                .expect("prearm plan implementation")
        );
        let (event_tx, event_rx) = mpsc::unbounded();
        let prompt = "Implement the reviewed plan exactly.";

        handle_hosted_plan_action(
            HostedPlanAction::ImplementApproved {
                prompt: prompt.to_string(),
                approval_mode: ApprovalMode::FullAuto,
            },
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
            &pending_workflow_notifications,
        );

        let events = event_rx.try_iter().collect::<Vec<_>>();
        let settings = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    TuiEvent::SettingsUpdated {
                        approval_mode: ApprovalMode::FullAuto,
                        ..
                    }
                )
            })
            .expect("settings update");
        let implementation = events
            .iter()
            .position(|event| {
                matches!(event, TuiEvent::PlanImplementationStarted {
                    prompt: event_prompt,
                } if event_prompt == prompt)
            })
            .expect("plan implementation start");
        let ready = events
            .iter()
            .position(|event| matches!(event, TuiEvent::MentionRuntimeReady(_)))
            .expect("runtime ready");
        let turn = events
            .iter()
            .position(|event| matches!(event, TuiEvent::TurnStarted { .. }))
            .unwrap_or_else(|| panic!("turn started; events: {events:?}"));
        let terminal = events
            .iter()
            .position(|event| {
                matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
            })
            .expect("successful terminal");
        assert!(settings < implementation, "events: {events:?}");
        assert!(implementation < ready, "events: {events:?}");
        assert!(ready < turn, "events: {events:?}");
        assert!(turn < terminal, "events: {events:?}");
        assert_eq!(
            config.lock().expect("config").approval_mode,
            ApprovalMode::FullAuto
        );
        let snapshot = crate::surface_actions::TuiSurfaceActions::new(
            thread
                .as_ref()
                .expect("started runtime thread")
                .typed_surface(),
        )
        .read_snapshot()
        .expect("typed surface snapshot");
        assert!(snapshot.items.iter().any(|item| {
            matches!(
                item,
                orca_runtime::surface::SurfaceItem::UserMessage {
                    input: orca_runtime::surface::SurfaceUserInputState::Resolved {
                        fact: orca_runtime::surface::SurfaceResolvedInputFact::Replayable {
                            input,
                            ..
                        },
                    },
                    ..
                } if input.canonical_text.as_str() == prompt
            )
        }));

        thread
            .take()
            .expect("started runtime thread")
            .shutdown()
            .expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }

    #[test]
    fn settings_rejection_releases_activation_without_starting_implementation() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let host = runtime.handle();
        let mut thread = Some(
            host.start_thread(
                config.lock().expect("config").clone(),
                "plan implementation rejection",
            )
            .expect("runtime thread"),
        );
        let preloaded = Arc::new(Mutex::new(None));
        let pending_workflow_notifications = bridge::PendingWorkflowNotifications::new();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        assert!(
            control
                .begin_surface_activation()
                .expect("prearm plan implementation")
        );
        let (event_tx, event_rx) = mpsc::unbounded();

        handle_hosted_plan_action(
            HostedPlanAction::ImplementApproved {
                prompt: "Implement the approved plan.".to_string(),
                approval_mode: ApprovalMode::FullAuto,
            },
            &mut thread,
            &host,
            &config,
            &preloaded,
            &event_tx,
            &control,
            &pending_workflow_notifications,
        );

        assert_eq!(
            config.lock().expect("config").approval_mode,
            ApprovalMode::Suggest
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "typed TUI settings attachment unavailable"
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(thread.is_some(), "settings rejection preserves the thread");
        assert!(
            control
                .begin_surface_activation()
                .expect("settings rejection releases activation")
        );
        control.cancel_surface_activation();

        thread
            .take()
            .expect("runtime thread")
            .shutdown()
            .expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
