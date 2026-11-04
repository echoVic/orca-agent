//! Hosted settings translation and application ownership.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::{ReasoningEffort, RunConfig};
use orca_runtime::runtime_host::RuntimeThreadHandle;
use orca_runtime::surface::RuntimeSettingsPatch;

use crate::protocol::TuiEvent;
use crate::slash_command_actions::SettingsIntent;
use crate::surface_actions::TuiSurfaceActions;

const QUEUED_SETTINGS_PREFIX: &str = "__orca_queued_settings__:";

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct QueuedSettings {
    pub sequence: u64,
    pub thread_id: Option<String>,
    pub encoded: String,
}

impl QueuedSettings {
    pub(crate) fn encode(&self) -> String {
        format!(
            "{QUEUED_SETTINGS_PREFIX}{}",
            serde_json::to_string(self).expect("settings envelope")
        )
    }

    pub(crate) fn decode(encoded: &str) -> Option<Self> {
        serde_json::from_str(encoded.strip_prefix(QUEUED_SETTINGS_PREFIX)?).ok()
    }
}

#[derive(Debug, Default)]
pub(crate) struct HostedSettingsOrder {
    confirmed_full_access: std::collections::HashMap<String, u64>,
}

impl HostedSettingsOrder {
    pub(crate) fn commit_hosted_settings(
        &mut self,
        thread: Option<&RuntimeThreadHandle>,
        config: &Arc<Mutex<RunConfig>>,
        event_tx: &mpsc::Sender<TuiEvent>,
        mut patches: Vec<RuntimeSettingsPatch>,
        queued: Option<&QueuedSettings>,
    ) -> bool {
        let thread_id = thread.map(RuntimeThreadHandle::thread_id);
        if let Some(queued) = queued {
            if queued
                .thread_id
                .as_deref()
                .is_some_and(|id| Some(id) != thread_id)
            {
                report_settings_rejection(
                    thread,
                    event_tx,
                    "settings target is no longer in view".into(),
                );
                return false;
            }
            if thread_id
                .and_then(|id| self.confirmed_full_access.get(id))
                .is_some_and(|confirmed| queued.sequence < *confirmed)
            {
                // Preserve independent model/effort changes from the old request.
                patches.retain(|patch| {
                    !matches!(
                        patch,
                        RuntimeSettingsPatch::SetApprovalMode { .. }
                            | RuntimeSettingsPatch::EnableFullAccess
                    )
                });
            }
        }
        let confirms_full_access = patches.contains(&RuntimeSettingsPatch::EnableFullAccess);
        let applied = apply_hosted_settings_action(thread, config, event_tx, patches);
        if applied
            && confirms_full_access
            && let (Some(thread_id), Some(queued)) = (thread_id, queued)
        {
            self.confirmed_full_access
                .insert(thread_id.to_string(), queued.sequence);
        }
        applied
    }
}

pub(crate) fn settings_intent_patches(
    intent: SettingsIntent,
) -> Vec<orca_runtime::surface::RuntimeSettingsPatch> {
    let mut patches = Vec::new();
    if let Some(model) = intent.model
        && let Ok(model) = orca_runtime::surface::NonEmptyText::try_new(
            orca_core::model::canonical_model_name(&model).to_string(),
        )
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
        match (mode, intent.full_access_confirmed) {
            (orca_core::approval_types::ApprovalMode::FullAuto, true) => {
                patches.push(orca_runtime::surface::RuntimeSettingsPatch::EnableFullAccess);
            }
            (orca_core::approval_types::ApprovalMode::FullAuto, false) => {}
            (mode, _) => {
                patches.push(
                    orca_runtime::surface::RuntimeSettingsPatch::SetApprovalMode {
                        mode: surface_approval_mode(mode),
                    },
                );
            }
        }
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

pub(crate) fn settings_updated_from_surface(
    settings: &orca_runtime::surface::SurfaceRuntimeSettings,
) -> Result<TuiEvent, String> {
    let reasoning_effort = match settings.reasoning_effort {
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
            return Err("runtime returned an unsupported reasoning effort".to_string());
        }
    };
    let approval_mode = match settings.approval_mode {
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
    Ok(TuiEvent::SettingsUpdated {
        model: settings.model.as_str().to_string(),
        reasoning_effort,
        approval_mode,
    })
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
    let unconfirmed_full_access = patches.iter().any(|patch| {
        matches!(
            patch,
            RuntimeSettingsPatch::SetApprovalMode {
                mode: orca_runtime::surface::SurfaceApprovalMode::FullAuto
            }
        )
    }) && config
        .lock()
        .map(|config| config.approval_mode != orca_core::approval_types::ApprovalMode::FullAuto)
        .unwrap_or(true);
    if unconfirmed_full_access {
        report_settings_rejection(
            thread,
            event_tx,
            "Full Access requires explicit confirmation".to_string(),
        );
        return false;
    }
    let persist_model = patches
        .iter()
        .any(|patch| matches!(patch, RuntimeSettingsPatch::SetModel { .. }));
    let persist_effort = patches
        .iter()
        .any(|patch| matches!(patch, RuntimeSettingsPatch::SetReasoning { .. }));
    let enables_full_access = patches
        .iter()
        .any(|patch| matches!(patch, RuntimeSettingsPatch::EnableFullAccess));
    if let Some(thread) = thread {
        let patches = match orca_runtime::surface::NonEmptyVec::try_new(patches) {
            Ok(patches) => patches,
            Err(_) => return false,
        };
        let actions = TuiSurfaceActions::new(thread.typed_surface());
        let settings = match actions.update_settings(patches) {
            Ok(settings) => settings,
            Err(error) => {
                report_settings_rejection(Some(thread), event_tx, error.to_string());
                return false;
            }
        };
        let settings_event = match settings_updated_from_surface(&settings.effective) {
            Ok(event) => event,
            Err(error) => {
                report_settings_rejection(Some(thread), event_tx, error);
                return false;
            }
        };
        let TuiEvent::SettingsUpdated {
            model,
            reasoning_effort,
            approval_mode,
        } = settings_event
        else {
            unreachable!("surface settings helper always returns SettingsUpdated");
        };
        if let Ok(mut cfg) = config.lock() {
            cfg.model = cfg.model.with_value_unchecked(Some(model.clone()));
            cfg.reasoning_effort = reasoning_effort;
            cfg.approval_mode = approval_mode;
            cfg.execution_profile =
                orca_core::capability::ExecutionProfile::for_approval_mode(approval_mode);
            if enables_full_access {
                cfg.active_permission_profile = None;
            }
        }
        let _ = event_tx.send(TuiEvent::SettingsUpdated {
            model: model.clone(),
            reasoning_effort,
            approval_mode,
        });
        persist_user_settings_values(
            persist_model.then_some(model.as_str()),
            persist_effort.then_some(reasoning_effort),
            event_tx,
        );
        return true;
    }

    let mut cfg = config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for patch in patches {
        match patch {
            orca_runtime::surface::RuntimeSettingsPatch::SetModel { model } => {
                cfg.model = cfg
                    .model
                    .with_value_unchecked(Some(model.as_str().to_string()));
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
                cfg.startup_approval_mode = Some(cfg.approval_mode);
                cfg.execution_profile =
                    orca_core::capability::ExecutionProfile::for_approval_mode(cfg.approval_mode);
            }
            orca_runtime::surface::RuntimeSettingsPatch::EnableFullAccess => {
                cfg.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
                cfg.startup_approval_mode = Some(cfg.approval_mode);
                cfg.execution_profile = orca_core::capability::ExecutionProfile::TrustedHost;
                cfg.active_permission_profile = None;
            }
            _ => {}
        }
    }
    let _ = event_tx.send(TuiEvent::SettingsUpdated {
        model: cfg.model.display_name().to_string(),
        reasoning_effort: cfg.reasoning_effort,
        approval_mode: cfg.approval_mode,
    });
    let model = persist_model.then(|| cfg.model.display_name().to_string());
    let effort = persist_effort.then_some(cfg.reasoning_effort);
    persist_user_settings_values(model.as_deref(), effort, event_tx);
    true
}

fn report_settings_rejection(
    thread: Option<&RuntimeThreadHandle>,
    event_tx: &mpsc::Sender<TuiEvent>,
    message: String,
) {
    // A rejected settings mutation does not end the active turn or answer
    // its pending interaction. OperationRejected would reset the UI to Idle.
    let event = if thread.is_some_and(RuntimeThreadHandle::is_busy) {
        TuiEvent::Error(message)
    } else {
        TuiEvent::OperationRejected(message)
    };
    let _ = event_tx.send(event);
}

/// Persist the applied model and reasoning-effort choices to the user-owned
/// config file. Persistence failure is surfaced as a notice; the in-session
/// change has already been committed and is not rolled back.
fn persist_user_settings_values(
    model: Option<&str>,
    effort: Option<ReasoningEffort>,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    if model.is_none() && effort.is_none() {
        return;
    }
    if let Err(error) = orca_core::config::file::persist_user_model_settings(model, effort) {
        let _ = event_tx.send(TuiEvent::Notice(format!(
            "Settings applied for this session, but saving them to the user config failed: {error}"
        )));
    }
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
            model: Some(orca_core::model::LEGACY_VISION_MODEL.to_string()),
            reasoning_effort: Some(ReasoningEffort::High),
            approval_mode: Some(ApprovalMode::Plan),
            full_access_confirmed: false,
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
            ] if model.as_str() == orca_core::model::FLASH_MODEL
        ));
    }

    #[test]
    fn confirmed_full_access_uses_the_authority_widening_patch() {
        let patches = super::settings_intent_patches(SettingsIntent {
            model: None,
            reasoning_effort: None,
            approval_mode: Some(ApprovalMode::FullAuto),
            full_access_confirmed: true,
        });

        assert_eq!(patches, vec![RuntimeSettingsPatch::EnableFullAccess]);
    }

    #[test]
    fn unconfirmed_full_access_intent_fails_closed() {
        let patches = super::settings_intent_patches(SettingsIntent {
            model: None,
            reasoning_effort: None,
            approval_mode: Some(ApprovalMode::FullAuto),
            full_access_confirmed: false,
        });

        assert!(patches.is_empty());
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
    fn prestart_mode_change_supersedes_launch_intent_before_first_turn() {
        let _home = crate::test_support::isolate_orca_home();
        for (initial_mode, patch, expected) in [
            (
                ApprovalMode::FullAuto,
                RuntimeSettingsPatch::SetApprovalMode {
                    mode: SurfaceApprovalMode::Suggest,
                },
                ApprovalMode::Suggest,
            ),
            (
                ApprovalMode::Suggest,
                RuntimeSettingsPatch::EnableFullAccess,
                ApprovalMode::FullAuto,
            ),
        ] {
            let mut cfg = crate::test_support::test_run_config();
            cfg.history_mode = orca_core::config::HistoryMode::Record;
            cfg.approval_mode = initial_mode;
            cfg.startup_approval_mode = Some(initial_mode);
            cfg.execution_profile =
                orca_core::capability::ExecutionProfile::for_approval_mode(initial_mode);
            let config = Arc::new(Mutex::new(cfg));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, _event_rx) = mpsc::unbounded();
            assert!(super::apply_hosted_settings_action(
                None,
                &config,
                &event_tx,
                vec![patch]
            ));
            assert!(super::apply_hosted_settings_action(
                None,
                &config,
                &event_tx,
                vec![RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::Low
                },]
            ));
            let host = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
            let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
            let mut thread = None;
            crate::hosted_session_lifecycle::ensure_hosted_thread(
                &mut thread,
                &host.handle(),
                &config,
                &preloaded,
                "first turn",
                &event_tx,
                &control,
            )
            .unwrap();
            let mode = config.lock().unwrap().approval_mode;
            thread.take().unwrap().shutdown().unwrap();
            host.shutdown().unwrap();
            assert_eq!(
                mode, expected,
                "the latest explicit mode must supersede launch intent"
            );
            assert_eq!(
                config.lock().unwrap().reasoning_effort,
                ReasoningEffort::Low
            );
        }
    }

    #[test]
    fn unconfirmed_direct_full_auto_patch_is_rejected() {
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(!super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::SetApprovalMode {
                mode: SurfaceApprovalMode::FullAuto,
            }],
        ));
        assert_ne!(
            config.lock().expect("config").approval_mode,
            ApprovalMode::FullAuto
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::OperationRejected(message))
                if message == "Full Access requires explicit confirmation"
        ));
    }

    #[test]
    fn unattached_settings_update_mutates_config_then_emits_result() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![
                RuntimeSettingsPatch::SetModel {
                    model: NonEmptyText::try_new(orca_core::model::LEGACY_FLASH_MODEL)
                        .expect("model"),
                },
                RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::Low,
                },
                RuntimeSettingsPatch::EnableFullAccess,
            ],
        ));

        let config = config.lock().expect("config");
        assert_eq!(config.model.display_name(), orca_core::model::FLASH_MODEL);
        assert_eq!(config.reasoning_effort, ReasoningEffort::Low);
        assert_eq!(config.approval_mode, ApprovalMode::FullAuto);
        drop(config);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SettingsUpdated {
                model,
                reasoning_effort: ReasoningEffort::Low,
                approval_mode: ApprovalMode::FullAuto,
            }) if model == orca_core::model::FLASH_MODEL
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

    #[test]
    fn unattached_model_update_persists_model_and_effort_to_user_config() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![
                RuntimeSettingsPatch::SetModel {
                    model: NonEmptyText::try_new(orca_core::model::FLASH_MODEL).expect("model"),
                },
                RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::Low,
                },
            ],
        ));

        let content =
            std::fs::read_to_string(_home.path().join("config.toml")).expect("config.toml written");
        assert!(
            content.contains(&format!("model = \"{}\"", orca_core::model::FLASH_MODEL)),
            "model missing from {content:?}"
        );
        assert!(
            content.contains("reasoning_effort = \"low\""),
            "effort missing from {content:?}"
        );
        assert!(
            event_rx
                .try_iter()
                .all(|event| !matches!(event, TuiEvent::Notice(_))),
            "unexpected persistence notice"
        );
    }

    #[test]
    fn unattached_approval_mode_update_does_not_write_user_config() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::EnableFullAccess],
        ));

        let config = config.lock().expect("config");
        assert_eq!(config.approval_mode, ApprovalMode::FullAuto);
        assert_eq!(
            config.execution_profile,
            orca_core::capability::ExecutionProfile::TrustedHost
        );
        assert!(config.active_permission_profile.is_none());
        drop(config);
        assert!(
            !_home.path().join("config.toml").exists(),
            "approval mode changes must not write the user config"
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SettingsUpdated {
                approval_mode: ApprovalMode::FullAuto,
                ..
            })
        ));
    }

    #[test]
    fn unattached_model_update_notices_when_config_cannot_be_persisted() {
        let _home = crate::test_support::isolate_orca_home();
        let broken = "model = [\n";
        std::fs::write(_home.path().join("config.toml"), broken).unwrap();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let (event_tx, event_rx) = mpsc::unbounded();

        assert!(super::apply_hosted_settings_action(
            None,
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::SetModel {
                model: NonEmptyText::try_new(orca_core::model::FLASH_MODEL).expect("model"),
            }],
        ));

        assert_eq!(
            std::fs::read_to_string(_home.path().join("config.toml")).unwrap(),
            broken,
            "broken config must be preserved"
        );
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(
                events.as_slice(),
                [
                    TuiEvent::SettingsUpdated { .. },
                    TuiEvent::Notice(message)
                ] if message.contains("saving them to the user config failed")
            ),
            "unexpected events: {events:?}"
        );
    }

    #[test]
    fn full_access_commit_and_mirror_wait_for_earlier_settings_publication() {
        let _home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = orca_core::config::HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().unwrap();
        let thread = runtime
            .handle()
            .start_thread(config.lock().unwrap().clone(), "settings ordering")
            .unwrap();
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        // A rendezvous channel holds the first transaction at publication.
        let (event_tx, event_rx) = mpsc::bounded(0);
        let first = {
            let control = control.clone();
            let thread = thread.clone();
            let config = config.clone();
            let event_tx = event_tx.clone();
            std::thread::spawn(move || {
                control.apply_ordered_settings(
                    Some(&thread),
                    &config,
                    &event_tx,
                    vec![RuntimeSettingsPatch::SetReasoning {
                        effort: orca_runtime::surface::SurfaceReasoningEffort::High,
                    }],
                    None,
                )
            })
        };
        let actions = crate::surface_actions::TuiSurfaceActions::new(thread.typed_surface());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if actions
                .read_snapshot()
                .unwrap()
                .settings
                .effective
                .reasoning_effort
                == orca_runtime::surface::SurfaceReasoningEffort::High
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "first settings commit"
            );
            std::thread::yield_now();
        }
        let (started_tx, started_rx) = mpsc::bounded(1);
        let second = {
            let control = control.clone();
            let thread = thread.clone();
            let config = config.clone();
            let event_tx = event_tx.clone();
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                control.apply_ordered_settings(
                    Some(&thread),
                    &config,
                    &event_tx,
                    vec![RuntimeSettingsPatch::EnableFullAccess],
                    None,
                )
            })
        };
        started_rx.recv().unwrap();
        // Read without consuming either publication. The second transaction
        // must not reach the runtime while the first still owns publication.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
        let mut observed_early_full_access = false;
        while std::time::Instant::now() < deadline {
            observed_early_full_access |= actions
                .read_snapshot()
                .unwrap()
                .settings
                .effective
                .approval_mode
                == SurfaceApprovalMode::FullAuto;
            std::thread::yield_now();
        }
        let first_event = event_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let second_event = event_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(first.join().unwrap());
        assert!(second.join().unwrap());
        let final_mode = actions
            .read_snapshot()
            .unwrap()
            .settings
            .effective
            .approval_mode;
        let mirrored_mode = config.lock().unwrap().approval_mode;
        thread.shutdown().unwrap();
        runtime.shutdown().unwrap();
        assert!(
            !observed_early_full_access,
            "Full Access overtook pending publication"
        );
        assert!(matches!(
            first_event,
            TuiEvent::SettingsUpdated {
                approval_mode: ApprovalMode::Suggest,
                ..
            }
        ));
        assert!(matches!(
            second_event,
            TuiEvent::SettingsUpdated {
                approval_mode: ApprovalMode::FullAuto,
                ..
            }
        ));
        assert_eq!(final_mode, SurfaceApprovalMode::FullAuto);
        assert_eq!(mirrored_mode, ApprovalMode::FullAuto);
    }

    #[test]
    fn attached_model_update_persists_after_runtime_commit() {
        let _home = crate::test_support::isolate_orca_home();
        let mut run_config = crate::test_support::test_run_config();
        run_config.history_mode = orca_core::config::HistoryMode::Record;
        let config = Arc::new(Mutex::new(run_config));
        let runtime = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = runtime
            .handle()
            .start_thread(config.lock().expect("config").clone(), "settings persist")
            .expect("runtime thread");
        let (event_tx, event_rx) = mpsc::unbounded();

        let applied = super::apply_hosted_settings_action(
            Some(&thread),
            &config,
            &event_tx,
            vec![RuntimeSettingsPatch::SetModel {
                model: NonEmptyText::try_new(orca_core::model::FLASH_MODEL).expect("model"),
            }],
        );
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(applied, "settings events: {events:?}");

        let content =
            std::fs::read_to_string(_home.path().join("config.toml")).expect("config.toml written");
        assert!(
            content.contains(&format!("model = \"{}\"", orca_core::model::FLASH_MODEL)),
            "model missing from {content:?}"
        );
        thread.shutdown().expect("runtime thread shutdown");
        runtime.shutdown().expect("runtime host shutdown");
    }
}
