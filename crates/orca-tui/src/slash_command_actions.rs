use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::commands::{self, GoalSlashCommand, QueueSlashCommand, SlashCommand, TrustSlashCommand};
use crate::protocol::TuiMemoryScope;
use crate::protocol::{GoalDraft, UserAction};
use crate::session_picker_actions::open_session_picker;
use crate::surface_actions::TuiHostActions;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus, ConfigDialog};
use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;

pub(crate) enum SlashOutcome {
    Continue,
    Prefill(String),
}

pub(crate) fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    _shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let command = commands::parse_with_cwd(text, &cwd)?;
    dispatch_slash_command(command, None, config, state, action_tx)
}

pub(crate) fn handle_composer_slash_command(
    visible_text: &str,
    expanded_text: &str,
    pending_pastes: &[(String, String)],
    config: &mut RunConfig,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match commands::parse_with_cwd(visible_text, &cwd) {
        Some(SlashCommand::Goal(GoalSlashCommand::Set(objective))) => {
            let draft = GoalDraft {
                objective: objective.clone(),
                pending_pastes: pending_pastes.to_vec(),
            };
            dispatch_slash_command(
                SlashCommand::Goal(GoalSlashCommand::Set(objective)),
                Some(draft),
                config,
                state,
                action_tx,
            )
        }
        Some(SlashCommand::Goal(GoalSlashCommand::Edit(objective))) => {
            let draft = GoalDraft {
                objective: objective.clone(),
                pending_pastes: pending_pastes.to_vec(),
            };
            dispatch_slash_command(
                SlashCommand::Goal(GoalSlashCommand::Edit(objective)),
                Some(draft),
                config,
                state,
                action_tx,
            )
        }
        _ => {
            let command = commands::parse_with_cwd(expanded_text, &cwd)?;
            dispatch_slash_command(command, None, config, state, action_tx)
        }
    }
}

fn dispatch_slash_command(
    command: SlashCommand,
    goal_draft: Option<GoalDraft>,
    config: &mut RunConfig,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut pending_settings_action = None;
    match command {
        SlashCommand::New => {
            if state.status == AppStatus::Idle {
                state.enter_running();
                let _ = action_tx.send(UserAction::NewSession);
            } else {
                state.push_message(ChatMessage::Error(
                    "finish or cancel the current work before starting a new conversation"
                        .to_string(),
                ));
            }
        }
        SlashCommand::Model(Some(model)) => match commands::validate_model(&model) {
            Ok(()) => {
                pending_settings_action = Some(UserAction::SetModel(model));
            }
            Err(error) => state.push_message(ChatMessage::Error(error)),
        },
        SlashCommand::Model(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current model: {} (reasoning effort: {}). Use the /model menu to change both.",
                state.model_name,
                state.reasoning_effort.as_str()
            )));
        }
        SlashCommand::Cost => {
            let usage = state.usage();
            state.push_message(ChatMessage::System(format!(
                "Session usage: {} input, {} output, {} cache tokens, estimated ${:.6}.",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_tokens,
                usage.estimated_cost_usd
            )));
        }
        SlashCommand::Config => {
            if state.status == AppStatus::Idle {
                state.config_dialog = Some(ConfigDialog {
                    selected: 0,
                    model: state.model_name.clone(),
                    reasoning_effort: state.reasoning_effort,
                    approval_mode: state.approval_mode,
                });
            } else {
                state.push_message(ChatMessage::Error(
                    "finish or cancel the current work before changing configuration".to_string(),
                ));
            }
        }
        SlashCommand::Mode(Some(mode)) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(approval_mode),
                )));
            }
            None => state.push_message(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, full-auto, or plan.".to_string(),
            )),
        },
        SlashCommand::Mode(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current mode: {}",
                state.approval_mode.as_str()
            )));
        }
        SlashCommand::Plan(arg) => match arg.as_deref() {
            Some("off") => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::default()),
                )));
            }
            None => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::Plan),
                )));
            }
            Some(_) => state.push_message(ChatMessage::Error(
                "unsupported plan command. Use /plan or /plan off.".to_string(),
            )),
        },
        SlashCommand::Goal(goal_command) => {
            let action = match goal_command {
                GoalSlashCommand::Show => UserAction::GoalShow,
                GoalSlashCommand::Set(objective) => {
                    UserAction::GoalSet(goal_draft.unwrap_or_else(|| GoalDraft {
                        objective,
                        pending_pastes: Vec::new(),
                    }))
                }
                GoalSlashCommand::Edit(objective) => {
                    UserAction::GoalEdit(goal_draft.unwrap_or_else(|| GoalDraft {
                        objective,
                        pending_pastes: Vec::new(),
                    }))
                }
                GoalSlashCommand::Clear => UserAction::GoalClear,
                GoalSlashCommand::Pause => UserAction::GoalPause,
                GoalSlashCommand::Resume => UserAction::GoalResume,
            };
            state.enter_running();
            let _ = action_tx.send(action);
        }
        SlashCommand::Queue(queue_command) => {
            let revision = state.runtime_queue_revision();
            let action = match queue_command {
                QueueSlashCommand::List => orca_runtime::prompt_queue::PromptQueueAction::List,
                QueueSlashCommand::Pause => orca_runtime::prompt_queue::PromptQueueAction::Pause {
                    expected_revision: revision,
                },
                QueueSlashCommand::Start => orca_runtime::prompt_queue::PromptQueueAction::Start {
                    expected_revision: revision,
                },
            };
            let _ = action_tx.send(UserAction::PromptQueueControl(action));
        }
        SlashCommand::SkillRun { id, args } => {
            let prompt = match args {
                Some(a) => format!("${id}:{a}"),
                None => format!("${id}"),
            };
            state.record_prompt(prompt.clone());
            state.push_message(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        SlashCommand::WorkflowList => {
            state.show_workflows();
        }
        SlashCommand::SkillList => return Some(SlashOutcome::Prefill("$".to_string())),
        SlashCommand::WorkflowRun { name, args } => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RunWorkflow { name, args });
        }
        SlashCommand::AgentDashboard => {
            state.show_agents();
        }
        SlashCommand::TaskWorkspace => {
            state.show_agents();
        }
        SlashCommand::TaskFollowUp { task_id, prompt } => {
            let _ = action_tx.send(UserAction::FollowUpTask { task_id, prompt });
        }
        SlashCommand::Remember(note) => {
            let (scope, note) = if let Some(project_note) = note.strip_prefix("project:") {
                (TuiMemoryScope::Project, project_note.trim().to_string())
            } else {
                (TuiMemoryScope::User, note)
            };
            let _ = action_tx.send(UserAction::Remember { scope, note });
        }
        SlashCommand::Compact => {
            state.enter_running();
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::Resume => match open_session_picker(state) {
            Ok(true) => {}
            Ok(false) => {
                state.push_message(ChatMessage::System("No saved conversations.".to_string()))
            }
            Err(error) => state.push_message(ChatMessage::Error(format!(
                "failed to list saved conversations: {error}"
            ))),
        },
        SlashCommand::Fork(title) => {
            if state.status == AppStatus::Idle {
                state.enter_running();
                let _ = action_tx.send(UserAction::ForkCurrentSession { title });
            } else {
                state.push_message(ChatMessage::Error(
                    "finish or cancel the current work before forking this conversation"
                        .to_string(),
                ));
            }
        }
        SlashCommand::Side(prompt) => {
            if state.side_conversation_available() && !state.side_conversation_active() {
                let _ = action_tx.send(UserAction::ToggleSideConversation);
            } else if state.side_conversation_active() {
                state.push_message(ChatMessage::Error(
                    "already in a side conversation; use Ctrl+C to close it".to_string(),
                ));
            } else if matches!(state.status, AppStatus::Setup | AppStatus::SessionPicker) {
                state.push_message(ChatMessage::Error(
                    "start the main conversation before opening a side conversation".to_string(),
                ));
            } else {
                if prompt.is_some() {
                    state.enter_running();
                }
                let _ = action_tx.send(UserAction::StartSideConversation { prompt });
            }
        }
        SlashCommand::Rename(None) => return Some(SlashOutcome::Prefill("/rename ".to_string())),
        SlashCommand::Rename(Some(title)) => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RenameCurrentSession { title });
        }
        SlashCommand::Status => {
            state.push_message(ChatMessage::System(format_status(state)));
        }
        SlashCommand::Copy(argument) => {
            let position = match argument.as_deref() {
                None => Some(1),
                Some(value) => value.parse::<usize>().ok().filter(|value| *value > 0),
            };
            match position.and_then(|position| {
                state
                    .nth_final_assistant_response(position)
                    .map(str::to_string)
            }) {
                Some(text) => state.stage_clipboard_copy(text, Instant::now()),
                None => state.push_message(ChatMessage::Error(
                    "usage: /copy [N], where N selects a completed assistant response from newest to oldest"
                        .to_string(),
                )),
            }
        }
        SlashCommand::CancelOperation => {
            if let Some(operation_id) = state.recoverable_operation_id().cloned() {
                state.enter_running();
                let _ = action_tx.send(UserAction::CancelOperation { operation_id });
            } else {
                state.push_message(ChatMessage::Error(
                    "no recoverable operation is available".to_string(),
                ));
            }
        }
        SlashCommand::Trust(trust_command) => match trust_command {
            TrustSlashCommand::Show => {
                if TuiHostActions::folder_is_trusted(&cwd) {
                    state.push_message(ChatMessage::System(format!(
                            "{} is trusted; the OS sandbox honors the configured write and network policy.",
                            cwd.display()
                        )))
                } else {
                    state.push_message(ChatMessage::System(format!(
                            "{} is not trusted; commands run read-only with no network. Use /trust add to trust it.",
                            cwd.display()
                        )))
                }
            }
            TrustSlashCommand::Add => match TuiHostActions::set_folder_trust(&cwd, true) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Trusted {}. Restart Orca to load project config from this folder.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to trust folder: {error}"
                ))),
            },
            TrustSlashCommand::Remove => match TuiHostActions::set_folder_trust(&cwd, false) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Removed trust for {}; commands now run read-only with no network.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to update trust: {error}"
                ))),
            },
        },
    }
    if let Some(action) = pending_settings_action {
        let _ = action_tx.send(action);
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

fn format_status(state: &AppState) -> String {
    let session_id = state.current_session_id().unwrap_or("-");
    let title = state.current_session_title().unwrap_or("-");
    let context_used_tokens = state.context_used_tokens();
    let context_limit_tokens = state.context_limit_tokens();
    let context = if context_limit_tokens == 0 {
        "-".to_string()
    } else {
        let used = context_used_tokens.min(context_limit_tokens);
        let remaining = context_limit_tokens.saturating_sub(used);
        format!("{remaining} remaining / {context_limit_tokens} total")
    };
    let usage = state.usage();
    let active_tasks = state
        .workflow_tasks()
        .iter()
        .filter(|task| {
            matches!(
                task.status,
                orca_core::task_types::TaskStatus::Queued
                    | orca_core::task_types::TaskStatus::Running
                    | orca_core::task_types::TaskStatus::Paused
                    | orca_core::task_types::TaskStatus::Stopping
                    | orca_core::task_types::TaskStatus::ApprovalRequired
            )
        })
        .count();
    let goal = state.current_goal().map_or("-", |goal| {
        orca_core::goal_types::goal_status_label(goal.status)
    });
    format!(
        "Session status\n\
         title: {title}\n\
         id: {session_id}\n\
         model: {} ({})\n\
         mode: {}\n\
         cwd: {}\n\
         context: {context}\n\
         usage: {} input, {} output, {} cache\n\
         cost: ${:.6}\n\
         goal: {goal}\n\
         active tasks: {active_tasks}\n\
         recoverable: {}",
        state.model_name,
        state.reasoning_effort.as_str(),
        state.approval_mode.as_str(),
        state.cwd,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_tokens,
        usage.estimated_cost_usd,
        if state.recoverable_operation_id().is_some() {
            "yes"
        } else {
            "no"
        },
    )
}

pub(crate) fn parse_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => None,
    }
}

const SETTINGS_INTENT_PREFIX: &str = "__orca_runtime_settings__:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingsIntent {
    pub model: Option<String>,
    pub reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    pub approval_mode: Option<ApprovalMode>,
}

pub(crate) fn encode_settings_intent(
    model: Option<&str>,
    reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    approval_mode: Option<ApprovalMode>,
) -> String {
    format!(
        "{SETTINGS_INTENT_PREFIX}{}|{}|{}",
        model.unwrap_or("-"),
        reasoning_effort.map_or("-", orca_core::config::ReasoningEffort::as_str),
        approval_mode.map_or("-", ApprovalMode::as_str),
    )
}

pub(crate) fn decode_settings_intent(value: &str) -> Option<SettingsIntent> {
    let fields = value
        .strip_prefix(SETTINGS_INTENT_PREFIX)?
        .split('|')
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return None;
    }
    let model = match fields[0] {
        "-" => None,
        model if orca_core::model::validate_model(model).is_ok() => Some(model.to_string()),
        _ => return None,
    };
    let reasoning_effort = match fields[1] {
        "-" => None,
        "low" => Some(orca_core::config::ReasoningEffort::Low),
        "high" => Some(orca_core::config::ReasoningEffort::High),
        "max" => Some(orca_core::config::ReasoningEffort::Max),
        _ => return None,
    };
    let approval_mode = match fields[2] {
        "-" => None,
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => return None,
    };
    (model.is_some() || reasoning_effort.is_some() || approval_mode.is_some()).then_some(
        SettingsIntent {
            model,
            reasoning_effort,
            approval_mode,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TuiEvent;
    use crate::surface_projection::SurfaceProjectionState;
    use crate::test_support::test_run_config;

    fn state() -> AppState {
        let (action_tx, _) = mpsc::unbounded();
        AppState::new(
            action_tx,
            "test".to_string(),
            "deepseek-v4-pro".to_string(),
            "/tmp/project".to_string(),
        )
    }

    #[test]
    fn low_reasoning_effort_round_trips_through_settings_intent() {
        let encoded = encode_settings_intent(
            Some("deepseek-v4-flash"),
            Some(orca_core::config::ReasoningEffort::Low),
            None,
        );

        let decoded = decode_settings_intent(&encoded).expect("decode low effort intent");

        assert_eq!(
            decoded.reasoning_effort,
            Some(orca_core::config::ReasoningEffort::Low)
        );
    }

    #[test]
    fn copy_slash_command_stages_nth_final_response() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("older".to_string()));
        state.push_message(ChatMessage::AssistantChunk {
            text: "unfinished".to_string(),
            trailing_blank: false,
        });
        state.push_message(ChatMessage::Assistant("latest".to_string()));
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _) = mpsc::unbounded();

        handle_slash_command("/copy 2", &mut config, &shared, &mut state, &action_tx);

        assert_eq!(
            state.viewport.pending_clipboard_copy.as_deref(),
            Some("older")
        );
    }

    #[test]
    fn copy_slash_command_rejects_invalid_or_missing_indices() {
        for command in ["/copy 0", "/copy nope", "/copy 2"] {
            let mut state = state();
            state.push_message(ChatMessage::Assistant("only".to_string()));
            let mut config = test_run_config();
            let shared = Arc::new(Mutex::new(config.clone()));
            let (action_tx, _) = mpsc::unbounded();

            handle_slash_command(command, &mut config, &shared, &mut state, &action_tx);

            assert!(
                state.viewport.pending_clipboard_copy.is_none(),
                "accepted {command}"
            );
            assert!(matches!(
                state.transcript.messages.last(),
                Some(ChatMessage::Error(_))
            ));
        }
    }

    #[test]
    fn status_slash_command_reports_session_snapshot() {
        let mut state = state();
        let recoverable_operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 3,
        ])
        .unwrap();
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("session-1".to_string()),
                title: "Release triage".to_string(),
                usage_revision: 1,
                usage: orca_core::cost_types::UsageTotals {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_tokens: 25,
                    estimated_cost_usd: 0.125,
                },
                context_revision: 1,
                context_used_tokens: 250,
                context_limit_tokens: 1_000,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: Some(recoverable_operation_id.clone()),
                recoverable_operation_id: Some(recoverable_operation_id),
                goal_presentation: None,
                session_presentation: None,
            },
        )));
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        state.approval_mode = ApprovalMode::Plan;
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, _) = mpsc::unbounded();

        handle_slash_command("/status", &mut config, &shared, &mut state, &action_tx);

        let Some(ChatMessage::System(status)) = state.transcript.messages.last() else {
            panic!("status output was not appended");
        };
        for expected in [
            "Release triage",
            "session-1",
            "deepseek-v4-pro",
            "plan",
            "/tmp/project",
            "750 remaining / 1000 total",
            "100 input, 50 output, 25 cache",
            "$0.125000",
            "recoverable: yes",
        ] {
            assert!(status.contains(expected), "missing {expected}: {status}");
        }
    }

    #[test]
    fn mode_and_plan_commands_use_committed_state_and_current_default() {
        let mut state = state();
        state.approval_mode = ApprovalMode::FullAuto;
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        handle_slash_command("/mode", &mut config, &shared, &mut state, &action_tx);

        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::System(message)) if message == "Current mode: full-auto"
        ));

        handle_slash_command("/plan off", &mut config, &shared, &mut state, &action_tx);
        let UserAction::SetModel(intent) = action_rx.try_recv().expect("plan settings action")
        else {
            panic!("expected settings action");
        };
        assert_eq!(
            decode_settings_intent(&intent)
                .expect("settings intent")
                .approval_mode,
            Some(ApprovalMode::default())
        );
    }

    #[test]
    fn fork_slash_command_dispatches_typed_action_only_while_idle() {
        for (status, should_dispatch) in [(AppStatus::Idle, true), (AppStatus::Running, false)] {
            let mut state = state();
            state.status = status;
            let mut config = test_run_config();
            let shared = Arc::new(Mutex::new(config.clone()));
            let (action_tx, action_rx) = mpsc::unbounded();

            handle_slash_command(
                "/fork auth experiment",
                &mut config,
                &shared,
                &mut state,
                &action_tx,
            );

            if should_dispatch {
                assert!(matches!(
                    action_rx.try_recv(),
                    Ok(UserAction::ForkCurrentSession { title: Some(title) })
                        if title == "auth experiment"
                ));
                assert_eq!(state.status, AppStatus::Running);
            } else {
                assert!(action_rx.try_recv().is_err());
                assert!(matches!(
                    state.transcript.messages.last(),
                    Some(ChatMessage::Error(_))
                ));
            }
        }
    }

    #[test]
    fn skills_slash_command_opens_picker_without_writing_transcript() {
        let mut state = state();
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        let outcome = handle_slash_command("/skills", &mut config, &shared, &mut state, &action_tx);

        assert!(matches!(
            outcome,
            Some(SlashOutcome::Prefill(value)) if value == "$"
        ));
        assert!(state.transcript.messages.is_empty());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn config_slash_command_opens_interactive_dialog_without_transcript_output() {
        let mut state = state();
        state.reasoning_effort = orca_core::config::ReasoningEffort::High;
        state.approval_mode = ApprovalMode::AutoEdit;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        let outcome = handle_slash_command("/config", &mut config, &shared, &mut state, &action_tx);

        assert!(matches!(outcome, Some(SlashOutcome::Continue)));
        let dialog = state.config_dialog.as_ref().expect("config dialog");
        assert_eq!(dialog.model, "deepseek-v4-pro");
        assert_eq!(
            dialog.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        assert_eq!(dialog.approval_mode, ApprovalMode::AutoEdit);
        assert!(state.transcript.messages.is_empty());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn rename_slash_command_prefills_or_dispatches_typed_action() {
        let mut state = state();
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::unbounded();

        assert!(matches!(
            handle_slash_command(
                "/rename",
                &mut config,
                &shared,
                &mut state,
                &action_tx,
            ),
            Some(SlashOutcome::Prefill(value)) if value == "/rename "
        ));
        assert!(action_rx.try_recv().is_err());

        handle_slash_command(
            "/rename release triage",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RenameCurrentSession { title }) if title == "release triage"
        ));
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn composer_goal_commands_preserve_visible_paste_bindings() {
        for (visible, is_edit) in [
            ("/goal [Pasted Content 1001 chars]", false),
            ("/goal edit [Pasted Content 1001 chars]", true),
        ] {
            let mut state = state();
            let mut config = test_run_config();
            let (action_tx, action_rx) = mpsc::unbounded();
            let pending = vec![("[Pasted Content 1001 chars]".to_string(), "x".repeat(1001))];
            let expanded = visible.replace(&pending[0].0, &pending[0].1);

            let outcome = handle_composer_slash_command(
                visible,
                &expanded,
                &pending,
                &mut config,
                &mut state,
                &action_tx,
            );

            assert!(matches!(outcome, Some(SlashOutcome::Continue)));
            let action = action_rx.try_recv().expect("Goal action");
            let draft = match action {
                UserAction::GoalSet(draft) if !is_edit => draft,
                UserAction::GoalEdit(draft) if is_edit => draft,
                other => panic!("unexpected Goal action: {other:?}"),
            };
            assert_eq!(draft.objective, pending[0].0);
            assert_eq!(draft.pending_pastes, pending);
        }
    }
}
