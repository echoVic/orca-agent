//! Hosted settings translation and application ownership.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::RunConfig;
use orca_runtime::runtime_host::RuntimeThreadHandle;

use crate::protocol::TuiEvent;
use crate::slash_command_actions::SettingsIntent;
use crate::surface_actions::TuiSurfaceActions;

pub(crate) fn settings_intent_patches(
    intent: SettingsIntent,
) -> Vec<orca_runtime::surface::RuntimeSettingsPatch> {
    let mut patches = Vec::new();
    if let Some(model) = intent.model
        && let Ok(model) = orca_runtime::surface::NonEmptyText::try_new(model)
    {
        patches.push(orca_runtime::surface::RuntimeSettingsPatch::SetModel { model });
    }
    if let Some(effort) = intent.reasoning_effort {
        patches.push(orca_runtime::surface::RuntimeSettingsPatch::SetReasoning {
            effort: match effort {
                orca_core::config::ReasoningEffort::Low => {
                    orca_runtime::surface::SurfaceReasoningEffort::Low
                }
                orca_core::config::ReasoningEffort::High => {
                    orca_runtime::surface::SurfaceReasoningEffort::High
                }
                orca_core::config::ReasoningEffort::Max => {
                    orca_runtime::surface::SurfaceReasoningEffort::Max
                }
            },
        });
    }
    if let Some(mode) = intent.approval_mode {
        patches.push(
            orca_runtime::surface::RuntimeSettingsPatch::SetApprovalMode {
                mode: surface_approval_mode(mode),
            },
        );
    }
    patches
}

pub(crate) fn surface_approval_mode(
    mode: orca_core::approval_types::ApprovalMode,
) -> orca_runtime::surface::SurfaceApprovalMode {
    match mode {
        orca_core::approval_types::ApprovalMode::Suggest => {
            orca_runtime::surface::SurfaceApprovalMode::Suggest
        }
        orca_core::approval_types::ApprovalMode::AutoEdit => {
            orca_runtime::surface::SurfaceApprovalMode::AutoEdit
        }
        orca_core::approval_types::ApprovalMode::FullAuto => {
            orca_runtime::surface::SurfaceApprovalMode::FullAuto
        }
        orca_core::approval_types::ApprovalMode::Plan => {
            orca_runtime::surface::SurfaceApprovalMode::Plan
        }
    }
}

pub(crate) fn apply_hosted_settings_action(
    thread: Option<&RuntimeThreadHandle>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    patches: Vec<orca_runtime::surface::RuntimeSettingsPatch>,
) -> bool {
    if patches.is_empty() {
        return false;
    }
    if let Some(thread) = thread {
        let patches = match orca_runtime::surface::NonEmptyVec::try_new(patches) {
            Ok(patches) => patches,
            Err(_) => return false,
        };
        let actions = TuiSurfaceActions::new(thread.typed_surface());
        let settings = match actions.update_settings(patches) {
            Ok(settings) => settings,
            Err(error) => {
                let _ = event_tx.send(TuiEvent::OperationRejected(error.to_string()));
                return false;
            }
        };
        let model = settings.effective.model.as_str().to_string();
        let reasoning_effort = match settings.effective.reasoning_effort {
            orca_runtime::surface::SurfaceReasoningEffort::Low => {
                orca_core::config::ReasoningEffort::Low
            }
            orca_runtime::surface::SurfaceReasoningEffort::High => {
                orca_core::config::ReasoningEffort::High
            }
            orca_runtime::surface::SurfaceReasoningEffort::Max => {
                orca_core::config::ReasoningEffort::Max
            }
            orca_runtime::surface::SurfaceReasoningEffort::Medium => {
                let _ = event_tx.send(TuiEvent::OperationRejected(
                    "runtime returned an unsupported reasoning effort".to_string(),
                ));
                return false;
            }
        };
        let approval_mode = match settings.effective.approval_mode {
            orca_runtime::surface::SurfaceApprovalMode::Suggest => {
                orca_core::approval_types::ApprovalMode::Suggest
            }
            orca_runtime::surface::SurfaceApprovalMode::AutoEdit => {
                orca_core::approval_types::ApprovalMode::AutoEdit
            }
            orca_runtime::surface::SurfaceApprovalMode::FullAuto => {
                orca_core::approval_types::ApprovalMode::FullAuto
            }
            orca_runtime::surface::SurfaceApprovalMode::Plan => {
                orca_core::approval_types::ApprovalMode::Plan
            }
        };
        if let Ok(mut cfg) = config.lock() {
            cfg.model = orca_core::model::ModelSelection::from_unchecked(Some(model.clone()));
            cfg.reasoning_effort = reasoning_effort;
            cfg.approval_mode = approval_mode;
        }
        let _ = event_tx.send(TuiEvent::SettingsUpdated {
            model,
            reasoning_effort,
            approval_mode,
        });
        return true;
    }

    let mut cfg = config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for patch in patches {
        match patch {
            orca_runtime::surface::RuntimeSettingsPatch::SetModel { model } => {
                cfg.model = orca_core::model::ModelSelection::from_unchecked(Some(
                    model.as_str().to_string(),
                ));
            }
            orca_runtime::surface::RuntimeSettingsPatch::SetReasoning { effort } => {
                cfg.reasoning_effort = match effort {
                    orca_runtime::surface::SurfaceReasoningEffort::Low => {
                        orca_core::config::ReasoningEffort::Low
                    }
                    orca_runtime::surface::SurfaceReasoningEffort::High => {
                        orca_core::config::ReasoningEffort::High
                    }
                    orca_runtime::surface::SurfaceReasoningEffort::Max => {
                        orca_core::config::ReasoningEffort::Max
                    }
                    orca_runtime::surface::SurfaceReasoningEffort::Medium => continue,
                };
            }
            orca_runtime::surface::RuntimeSettingsPatch::SetApprovalMode { mode } => {
                cfg.approval_mode = match mode {
                    orca_runtime::surface::SurfaceApprovalMode::Suggest => {
                        orca_core::approval_types::ApprovalMode::Suggest
                    }
                    orca_runtime::surface::SurfaceApprovalMode::AutoEdit => {
                        orca_core::approval_types::ApprovalMode::AutoEdit
                    }
                    orca_runtime::surface::SurfaceApprovalMode::FullAuto => {
                        orca_core::approval_types::ApprovalMode::FullAuto
                    }
                    orca_runtime::surface::SurfaceApprovalMode::Plan => {
                        orca_core::approval_types::ApprovalMode::Plan
                    }
                };
            }
            _ => {}
        }
    }
    let _ = event_tx.send(TuiEvent::SettingsUpdated {
        model: cfg.model.display_name().to_string(),
        reasoning_effort: cfg.reasoning_effort,
        approval_mode: cfg.approval_mode,
    });
    true
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossbeam_channel as mpsc;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::ReasoningEffort;
    use orca_runtime::surface::{
        NonEmptyText, RuntimeSettingsPatch, SurfaceApprovalMode, SurfaceReasoningEffort,
    };

    use crate::protocol::TuiEvent;
    use crate::slash_command_actions::SettingsIntent;

    #[test]
    fn settings_intent_preserves_model_reasoning_approval_patch_order() {
        let patches = super::settings_intent_patches(SettingsIntent {
            model: Some("deepseek-chat".to_string()),
            reasoning_effort: Some(ReasoningEffort::High),
            approval_mode: Some(ApprovalMode::Plan),
        });

        assert!(matches!(
            patches.as_slice(),
            [
                RuntimeSettingsPatch::SetModel { model },
                RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::High
                },
                RuntimeSettingsPatch::SetApprovalMode {
                    mode: SurfaceApprovalMode::Plan
                }
            ] if model.as_str() == "deepseek-chat"
        ));
    }

    #[test]
    fn empty_settings_update_is_rejected_without_config_or_event_mutation() {
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let before = config.lock().expect("config").clone();
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(!super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            Vec::new(),
        ));

        let after = config.lock().expect("config");
        assert_eq!(after.model.display_name(), before.model.display_name());
        assert_eq!(after.reasoning_effort, before.reasoning_effort);
        assert_eq!(after.approval_mode, before.approval_mode);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn unattached_settings_update_mutates_config_then_emits_result() {
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![
                RuntimeSettingsPatch::SetModel {
                    model: NonEmptyText::try_new("deepseek-chat").expect("model"),
                },
                RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::Low,
                },
                RuntimeSettingsPatch::SetApprovalMode {
                    mode: SurfaceApprovalMode::FullAuto,
                },
            ],
        ));

        let config = config.lock().expect("config");
        assert_eq!(config.model.display_name(), "deepseek-chat");
        assert_eq!(config.reasoning_effort, ReasoningEffort::Low);
        assert_eq!(config.approval_mode, ApprovalMode::FullAuto);
        drop(config);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SettingsUpdated {
                model,
                reasoning_effort: ReasoningEffort::Low,
                approval_mode: ApprovalMode::FullAuto,
            }) if model == "deepseek-chat"
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn attached_settings_update_commits_runtime_then_mirrors_effective_result() {
        let _home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = orca_core::config::HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = runtime
            .handle()
            .start_thread(config.lock().expect("config").clone(), "settings test")
            .expect("runtime thread");
        let (event_tx, event_rx) = mpsc::unbounded();

        let applied = super::apply_hosted_settings_action(
            Some(&thread),
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::SetApprovalMode {
                mode: SurfaceApprovalMode::Plan,
            }],
        );
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(applied, "settings events: {events:?}");

        assert_eq!(
            config.lock().expect("config").approval_mode,
            ApprovalMode::Plan
        );
        assert!(matches!(
            events.as_slice(),
            [TuiEvent::SettingsUpdated {
                approval_mode: ApprovalMode::Plan,
                ..
            }]
        ));
        thread.shutdown().expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }

    #[test]
    fn sessionless_attached_update_rejects_without_local_mirror() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = runtime
            .handle()
            .start_thread(config.lock().expect("config").clone(), "settings rejection")
            .expect("runtime thread");
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(!super::apply_hosted_settings_action(
            Some(&thread),
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::SetApprovalMode {
                mode: SurfaceApprovalMode::Plan,
            }],
        ));

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
        thread.shutdown().expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
