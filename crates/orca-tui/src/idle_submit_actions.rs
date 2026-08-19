use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::commands;
use crate::composer_textarea::{
    expand_pending_pastes, make_textarea, make_textarea_with_text, textarea_text,
};
use crate::slash_command_actions::{SlashOutcome, handle_slash_command};
use crate::theme::Theme;
use crate::types::{
    AppState, AppStatus, ChatMessage, PendingTuiInput, TuiInteractionResponse,
    TuiMcpElicitationMode, UserAction,
};
use crate::vim::VimState;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_submit(
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    state.slash_menu = None;
    let visible_text = textarea_text(textarea);
    state.mention_bindings.reconcile(&visible_text);
    let pending_interaction_composer = (state.status == AppStatus::WaitingUserInput).then(|| {
        (
            state.mention_bindings.clone(),
            state.atomic_skill_tokens.clone(),
            state.pending_pastes.clone(),
        )
    });
    let expanded_text = expand_pending_pastes(&visible_text, &state.pending_pastes);
    state.mention_bindings.reconcile(&expanded_text);
    let text = expanded_text.trim().to_string();
    state.mention_bindings.reconcile(&text);
    let empty_mcp_url_response = text.is_empty()
        && state.status == AppStatus::WaitingUserInput
        && matches!(
            state.pending_input,
            Some(PendingTuiInput::McpElicitation(_))
        )
        && matches!(
            state.pending_mcp_elicitation_mode,
            Some(TuiMcpElicitationMode::Url)
        );
    if text.is_empty() && !empty_mcp_url_response {
        return false;
    }

    if state.status != AppStatus::WaitingUserInput
        && let Some(outcome) = handle_slash_command(&text, config, shared_config, state, action_tx)
    {
        match outcome {
            SlashOutcome::Continue => {
                state.pending_pastes.clear();
                state.mention_bindings.clear();
                state.atomic_skill_tokens.clear();
                reset_composer_after_submit(textarea, vim_state, theme);
                return true;
            }
            SlashOutcome::Prefill(value) => {
                state.pending_pastes.clear();
                state.mention_bindings.clear();
                state.atomic_skill_tokens.clear();
                *textarea = make_textarea_with_text(&value, vim_state, theme);
                return true;
            }
        }
    }

    if state.status != AppStatus::WaitingUserInput && text.starts_with('/') {
        state.push_message(ChatMessage::Error(commands::invalid_slash_command_message(
            &text,
        )));
        state.pending_pastes.clear();
        state.mention_bindings.clear();
        state.atomic_skill_tokens.clear();
        reset_composer_after_submit(textarea, vim_state, theme);
        return true;
    }

    if state.status == AppStatus::WaitingUserInput {
        let response = match state.pending_input.as_ref() {
            Some(PendingTuiInput::UserInput(key)) => {
                Some((key.clone(), TuiInteractionResponse::UserInput(text)))
            }
            Some(PendingTuiInput::McpElicitation(key)) => {
                let content_json = if text.is_empty() {
                    "{}".to_string()
                } else {
                    if let Err(error) = serde_json::from_str::<serde_json::Value>(&text) {
                        state.push_message(ChatMessage::Error(format!(
                            "invalid typed MCP elicitation content: {error}"
                        )));
                        return true;
                    }
                    text
                };
                Some((
                    key.clone(),
                    TuiInteractionResponse::McpElicitation {
                        accepted: true,
                        content_json: Some(content_json),
                    },
                ))
            }
            None => None,
        };
        state.enter_running();
        state.scroll_to_bottom();
        if let Some((key, response)) = response {
            let (mention_bindings, atomic_skill_tokens, pending_pastes) =
                pending_interaction_composer.expect("waiting interaction captured composer state");
            let staged_key = state.stage_pending_interaction_submission_with_composer(
                visible_text.clone(),
                mention_bindings,
                atomic_skill_tokens,
                pending_pastes,
            );
            debug_assert_eq!(staged_key.as_ref(), Some(&key));
            state.pending_input = None;
            state.pending_mcp_elicitation_mode = None;
            let _ = action_tx.send(UserAction::RespondToInteraction { key, response });
        }
    } else {
        state.resume_queued_follow_up_autosend();
        state.record_prompt(text.clone());
        state.push_message(ChatMessage::User(visible_text.trim().to_string()));
        state.enter_running();
        state.scroll_to_bottom();
        let bindings = state.mention_bindings.clone();
        let _ = action_tx.send(UserAction::SubmitWithMentions {
            prompt: text,
            bindings,
        });
    }
    state.pending_pastes.clear();
    state.mention_bindings.clear();
    state.atomic_skill_tokens.clear();
    reset_composer_after_submit(textarea, vim_state, theme);
    true
}

pub(crate) fn submit_pending_user_input_choice(
    answer: String,
    textarea: &TextArea,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    let Some(PendingTuiInput::UserInput(key)) = state.pending_input.as_ref() else {
        return false;
    };
    let key = key.clone();
    let visible_text = textarea_text(textarea);
    let staged_key = state.stage_pending_interaction_submission_with_composer(
        visible_text,
        state.mention_bindings.clone(),
        state.atomic_skill_tokens.clone(),
        state.pending_pastes.clone(),
    );
    debug_assert_eq!(staged_key.as_ref(), Some(&key));
    state.pending_input = None;
    state.pending_mcp_elicitation_mode = None;
    state.user_input_dialog = None;
    state.enter_running();
    state.scroll_to_bottom();
    let _ = action_tx.send(UserAction::RespondToInteraction {
        key,
        response: TuiInteractionResponse::UserInput(answer),
    });
    true
}

fn reset_composer_after_submit(textarea: &mut TextArea, vim_state: &mut VimState, theme: &Theme) {
    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea(vim_state, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::test_support::test_run_config;
    use crate::types::{TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiMcpElicitationMode};
    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::ThemeName;

    fn interaction_key(kind: TuiInteractionKind, request_id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(OperationIdAllocator::default().allocate(), request_id, kind)
    }

    #[test]
    fn idle_submit_resumes_queued_autosend() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.suspend_queued_follow_up_autosend();
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("new foreground", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));
        assert!(state.queued_autosend_enabled());
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. })
                if prompt == "new foreground"
        ));
    }

    #[test]
    fn malformed_workflow_command_is_not_sent_to_the_model() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/workflow audit", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message))
                if message.contains("/workflow:<name>")
        ));
    }

    #[test]
    fn unknown_slash_command_is_not_sent_to_the_model() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/does-not-exist", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message)) if message.contains("unknown slash command")
        ));
    }

    #[test]
    fn waiting_user_input_treats_known_slash_command_as_literal_answer() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let key = interaction_key(TuiInteractionKind::UserInput, "input-slash");
        state.update(TuiEvent::UserInputRequested {
            key: key.clone(),
            question: "Which path?".to_string(),
            choices: Vec::new(),
        });
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/new", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserInput(answer),
            }) if actual_key == key && answer == "/new"
        ));
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.pending_input.is_none());
        assert_eq!(textarea_text(&textarea), "");
    }

    #[test]
    fn invalid_mcp_form_json_preserves_pending_input_and_composer() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let key = interaction_key(TuiInteractionKind::McpElicitation, "mcp-form");
        state.update(TuiEvent::McpElicitationRequested {
            key: key.clone(),
            server_name: "fixture".to_string(),
            mode: TuiMcpElicitationMode::Form,
            message: "Provide fields".to_string(),
            url: None,
            requested_schema_json: Some(r#"{"type":"object"}"#.to_string()),
        });
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("not-json", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert!(matches!(
            state.pending_input.as_ref(),
            Some(PendingTuiInput::McpElicitation(actual_key)) if actual_key == &key
        ));
        assert_eq!(textarea_text(&textarea), "not-json");
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message))
                if message.starts_with("invalid typed MCP elicitation content:")
        ));
    }

    #[test]
    fn empty_mcp_url_accepts_with_empty_json_object() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let key = interaction_key(TuiInteractionKind::McpElicitation, "mcp-url");
        state.update(TuiEvent::McpElicitationRequested {
            key: key.clone(),
            server_name: "fixture".to_string(),
            mode: TuiMcpElicitationMode::Url,
            message: "Authorize device".to_string(),
            url: Some("https://example.test/device".to_string()),
            requested_schema_json: None,
        });
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("", &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::McpElicitation {
                    accepted: true,
                    content_json: Some(content),
                },
            }) if actual_key == key && content == "{}"
        ));
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.pending_input.is_none());
        assert_eq!(textarea_text(&textarea), "");
    }

    #[test]
    fn empty_user_and_mcp_form_inputs_remain_pending() {
        let user_key = interaction_key(TuiInteractionKind::UserInput, "empty-user");
        let form_key = interaction_key(TuiInteractionKind::McpElicitation, "empty-form");
        let cases = [
            (
                TuiEvent::UserInputRequested {
                    key: user_key.clone(),
                    question: "Continue?".to_string(),
                    choices: Vec::new(),
                },
                user_key,
                None,
            ),
            (
                TuiEvent::McpElicitationRequested {
                    key: form_key.clone(),
                    server_name: "fixture".to_string(),
                    mode: TuiMcpElicitationMode::Form,
                    message: "Provide fields".to_string(),
                    url: None,
                    requested_schema_json: None,
                },
                form_key,
                Some(TuiMcpElicitationMode::Form),
            ),
        ];

        for (event, key, expected_mode) in cases {
            let (action_tx, action_rx) = mpsc::unbounded();
            let mut state = AppState::new(
                action_tx.clone(),
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            );
            state.update(event);
            let mut config = test_run_config();
            let shared = Arc::new(Mutex::new(config.clone()));
            let theme = Theme::named(ThemeName::Dark);
            let mut vim = VimState::new(false);
            let mut textarea = make_textarea_with_text("", &vim, &theme);

            assert!(!handle_idle_submit(
                &mut textarea,
                &mut vim,
                &theme,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
            ));

            assert!(action_rx.try_recv().is_err());
            assert_eq!(state.status, AppStatus::WaitingUserInput);
            assert_eq!(
                state.pending_input.as_ref().map(PendingTuiInput::key),
                Some(&key)
            );
            assert_eq!(state.pending_mcp_elicitation_mode, expected_mode);
            assert_eq!(textarea_text(&textarea), "");
        }
    }
}
