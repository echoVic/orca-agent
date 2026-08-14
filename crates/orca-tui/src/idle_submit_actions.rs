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
    AppState, AppStatus, ChatMessage, PendingTuiInput, TuiInteractionResponse, UserAction,
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
    let expanded_text = expand_pending_pastes(&visible_text, &state.pending_pastes);
    state.mention_bindings.reconcile(&expanded_text);
    let text = expanded_text.trim().to_string();
    state.mention_bindings.reconcile(&text);
    if text.is_empty() {
        return false;
    }

    if let Some(outcome) = handle_slash_command(&text, config, shared_config, state, action_tx) {
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
        state.enter_running();
        state.scroll_to_bottom();
        if let Some(pending) = state.pending_input.take() {
            let (key, response) = match pending {
                PendingTuiInput::UserInput(key) => (key, TuiInteractionResponse::UserInput(text)),
                PendingTuiInput::McpElicitation(key) => (
                    key,
                    TuiInteractionResponse::McpElicitation {
                        accepted: true,
                        content_json: Some(text),
                    },
                ),
            };
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

fn reset_composer_after_submit(textarea: &mut TextArea, vim_state: &mut VimState, theme: &Theme) {
    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea(vim_state, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::make_textarea_with_text;
    use crate::test_support::test_run_config;
    use orca_core::config::ThemeName;

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
}
