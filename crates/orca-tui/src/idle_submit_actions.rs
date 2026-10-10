use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use ratatui_textarea::TextArea;

use orca_core::config::RunConfig;
use orca_runtime::mentions::MentionBindings;

use crate::commands;
use crate::composer_images::{ComposerImageAttachment, ComposerImageState, DeferredImageSubmit};
use crate::composer_input_actions::sync_vim_mode_label;
use crate::composer_textarea::{
    MAX_USER_INPUT_TEXT_CHARS, expand_pending_pastes, make_textarea, make_textarea_with_text,
    textarea_text,
};
use crate::protocol::TuiUserInputResponse;
use crate::protocol::{
    PendingTuiInput, SubmitToken, TuiInteractionResponse, TuiMcpElicitationMode, UserAction,
};
use crate::queued_input::HeldSubmission;
use crate::queued_input_actions::{FollowUp, dispatch_follow_up};
use crate::slash_command_actions::{SlashOutcome, handle_composer_slash_command};
use crate::theme::Theme;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus};
use crate::vim::VimState;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_submit(
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    state: &mut AppState,
    config: &mut RunConfig,
    _shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    if state.composer_images.is_paste_in_flight() {
        state
            .composer_images
            .defer_submit(DeferredImageSubmit::Submit);
        return true;
    }
    state.slash_menu = None;
    let visible_text = textarea_text(textarea);
    let images = state.composer_images.attachments_for_text(&visible_text);
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
            state.interaction.pending_input,
            Some(PendingTuiInput::McpElicitation(_))
        )
        && matches!(
            state.interaction.pending_mcp_elicitation_mode,
            Some(TuiMcpElicitationMode::Url)
        );
    if text.is_empty() && !empty_mcp_url_response {
        return false;
    }

    if state.status != AppStatus::WaitingUserInput && !images.is_empty() && text.starts_with('/') {
        state.push_message(ChatMessage::Error(
            "remove image attachments before running a slash command".to_string(),
        ));
        return true;
    }
    if state.status != AppStatus::WaitingUserInput
        && let pending_pastes = state.pending_pastes.clone()
        && let Some(outcome) = handle_composer_slash_command(
            visible_text.trim(),
            &text,
            &pending_pastes,
            config,
            state,
            action_tx,
        )
    {
        match outcome {
            SlashOutcome::Continue => {
                state.pending_pastes.clear();
                state.composer_images.clear_attachments();
                state.mention_bindings.clear();
                state.atomic_skill_tokens.clear();
                reset_composer_after_submit(textarea, vim_state, theme, state);
                return true;
            }
            SlashOutcome::Prefill(value) => {
                state.pending_pastes.clear();
                state.composer_images.clear_attachments();
                state.mention_bindings.clear();
                state.atomic_skill_tokens.clear();
                *textarea = make_textarea_with_text(&value, vim_state, theme);
                return true;
            }
        }
    }

    if state.status != AppStatus::WaitingUserInput && text.starts_with('/') {
        state.push_message(ChatMessage::Diagnostic(
            crate::diagnostics::TuiDiagnostic::invalid_input(
                commands::invalid_slash_command_message(&text),
            ),
        ));
        state.pending_pastes.clear();
        state.composer_images.clear_attachments();
        state.mention_bindings.clear();
        state.atomic_skill_tokens.clear();
        reset_composer_after_submit(textarea, vim_state, theme, state);
        return true;
    }

    if state.status != AppStatus::WaitingUserInput {
        let actual_chars = text.chars().count();
        if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
            state.push_message(ChatMessage::Error(format!(
                "Message exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters ({actual_chars} provided)."
            )));
            return true;
        }
    }

    if state.status == AppStatus::WaitingUserInput {
        let response = match state.interaction.pending_input.as_ref() {
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
            state.interaction.pending_input = None;
            state.interaction.pending_mcp_elicitation_mode = None;
            let _ = action_tx.send(UserAction::RespondToInteraction { key, response });
        }
    } else {
        let history_text = ComposerImageState::text_without_labels(&text, &images);
        if !history_text.is_empty() {
            state.record_prompt(history_text);
        }
        let visible_text = visible_text.trim().to_string();
        let bindings = state.mention_bindings.clone();
        let pending_pastes = state.pending_pastes.clone();
        submit_user_message(
            state,
            action_tx,
            visible_text,
            text,
            bindings,
            images,
            pending_pastes,
        );
    }
    state.pending_pastes.clear();
    state.composer_images.clear_attachments();
    state.mention_bindings.clear();
    state.atomic_skill_tokens.clear();
    reset_composer_after_submit(textarea, vim_state, theme, state);
    true
}

pub(crate) fn submit_pending_user_input_response(
    response: TuiUserInputResponse,
    textarea: &TextArea,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    let Some(PendingTuiInput::UserInput(key)) = state.interaction.pending_input.as_ref() else {
        return false;
    };
    let key = key.clone();
    let response_summary = state
        .user_input_dialog
        .as_ref()
        .map(|dialog| dialog.response_summary(&response));
    let visible_text = textarea_text(textarea);
    let staged_key = state.stage_pending_interaction_submission_with_composer(
        visible_text,
        state.mention_bindings.clone(),
        state.atomic_skill_tokens.clone(),
        state.pending_pastes.clone(),
    );
    debug_assert_eq!(staged_key.as_ref(), Some(&key));
    if let Some(submission) = state.interaction.pending_submission.as_mut() {
        submission.response_summary = response_summary;
    }
    state.interaction.pending_input = None;
    state.interaction.pending_mcp_elicitation_mode = None;
    state.user_input_dialog = None;
    state.enter_running();
    state.scroll_to_bottom();
    let _ = action_tx.send(UserAction::RespondToInteraction {
        key,
        response: TuiInteractionResponse::UserQuestionnaire(response),
    });
    true
}

/// Sends `prompt`, with its mention `bindings` and attached `images`, as the
/// user's next message between turns; the conversation shows it as
/// `visible_text` with the images. A paused queue starts again, since the
/// user took over. `pending_pastes` are the composer's, which `prompt` has
/// expanded.
///
/// While the messages the user sends are held (see
/// [`AppState::holds_submissions`]) it only keeps this one, to send it
/// after the turn the held ones wait for, in order.
pub(crate) fn submit_user_message(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    visible_text: String,
    prompt: String,
    bindings: MentionBindings,
    images: Vec<ComposerImageAttachment>,
    pending_pastes: Vec<(String, String)>,
) {
    if state.holds_submissions() {
        state.held_submissions.push(HeldSubmission::Plain {
            visible_text,
            prompt,
            bindings,
            images,
            pending_pastes,
        });
        return;
    }
    send_user_message(
        state,
        action_tx,
        visible_text,
        prompt,
        bindings,
        images,
        None,
    );
}

/// What [`submit_user_message`] does when nothing is held back. `token` names
/// the message to the controller, which says it back when the message's turn
/// is active or will not be: see [`AppState::startup_turn`].
fn send_user_message(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    visible_text: String,
    prompt: String,
    bindings: MentionBindings,
    images: Vec<ComposerImageAttachment>,
    token: Option<SubmitToken>,
) {
    state.push_user_message_with_images(visible_text, &images);
    state.enter_running();
    state.scroll_to_bottom();
    let _ = action_tx.send(UserAction::SubmitWithMentions {
        prompt,
        bindings,
        images,
        token,
    });
    state.request_runtime_queue_start();
    state.resume_queued_follow_up_autosend();
}

/// The wait for the history of the conversation resumed at launch is over: it
/// has loaded, or a new conversation has taken its place. Sends the prompt
/// given on the command line, if there is one. If the messages the user sent
/// meanwhile were held, the prompt's turn is the one they wait for, and it is
/// named by a token that the controller says back when that turn is active
/// (see [`AppState::startup_turn`]). Without a prompt, the first held message
/// starts its turn instead.
pub(crate) fn start_turn_after_history(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    prompt: Option<String>,
) {
    let waited_for_history = std::mem::take(&mut state.startup_history_pending);
    match prompt {
        Some(prompt) => {
            let token = waited_for_history.then(|| state.new_submit_token());
            if token.is_some() {
                state.startup_turn = token;
            }
            let bindings = MentionBindings::new(&prompt);
            send_user_message(
                state,
                action_tx,
                prompt.clone(),
                prompt,
                bindings,
                Vec::new(),
                token,
            );
        }
        None if waited_for_history => start_held_turn(state, action_tx),
        None => {}
    }
}

/// Starts the turn of the first message held, as the composer starts the turn
/// of a message it sends between turns, and holds the ones after it until that
/// turn's operation is active: see [`release_held_submissions`]. With none
/// held, there is no turn to wait for, and nothing is held any more.
///
/// For when the turn the held messages waited for did not start, and when
/// there is no prompt to start one.
pub(crate) fn start_held_turn(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>) {
    if state.held_submissions.is_empty() {
        state.startup_turn = None;
        return;
    }
    let (visible_text, prompt, bindings, images) = state.held_submissions.remove(0).into_plain();
    let token = state.new_submit_token();
    state.startup_turn = Some(token);
    send_user_message(
        state,
        action_tx,
        visible_text,
        prompt,
        bindings,
        images,
        Some(token),
    );
}

/// The operation of the submit named `token` is active. If its turn is the one
/// the held messages wait for, queues them behind it, in order, as the
/// composer queues a message sent while a turn runs: the queue shows them from
/// then on, and they run after it, ahead of what is sent later. Nothing is
/// held any more. For any other token, or none, nothing happens.
pub(crate) fn release_held_submissions(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    token: Option<SubmitToken>,
) {
    if !state.waits_for_turn_of(token) {
        return;
    }
    state.startup_turn = None;
    for held in std::mem::take(&mut state.held_submissions) {
        let text = held.visible_text().to_string();
        let queued = held
            .into_follow_up()
            .is_some_and(|message| dispatch_follow_up(state, action_tx, message, FollowUp::Queue));
        if !queued {
            // The composer was cleared when it was sent: say what was lost.
            state.report_queued_input_error(format!("could not queue \"{text}\""));
        }
    }
}

/// The submit named `token` did not start a turn. If it is the one the held
/// messages wait for, the next of them starts its own turn, and the rest wait
/// for that; with none held, the hold is over. For any other token, or none,
/// nothing happens.
pub(crate) fn continue_held_submissions(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    token: Option<SubmitToken>,
) {
    if state.waits_for_turn_of(token) {
        start_held_turn(state, action_tx);
    }
}

fn reset_composer_after_submit(
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    state: &mut AppState,
) {
    vim_state.reset_insert(textarea, theme);
    sync_vim_mode_label(state, vim_state);
    *textarea = make_textarea(vim_state, theme);
    state.cleared_draft = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::protocol::{TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiMcpElicitationMode};
    use crate::test_support::test_run_config;
    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::ThemeName;

    fn interaction_key(kind: TuiInteractionKind, request_id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(OperationIdAllocator::default().allocate(), request_id, kind)
    }

    fn attach_test_image(state: &mut AppState, textarea: &mut TextArea) {
        state.model_name = orca_core::model::VISION_MODEL.to_string();
        let request = state.composer_images.begin_paste().unwrap();
        let (insertion, _, _) = state
            .composer_images
            .complete_paste(
                request,
                &textarea_text(textarea),
                crate::composer_textarea::textarea_cursor_byte_index(textarea),
                vec![crate::clipboard_image::ClipboardImagePayload {
                    media_type: "image/png".to_string(),
                    data: b"\x89PNG\r\n\x1a\nfixture".to_vec(),
                    width: 2,
                    height: 1,
                    source_name: None,
                }],
            )
            .unwrap();
        assert!(textarea.insert_str(&insertion));
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
    fn image_only_submit_carries_attachment_and_clears_composer() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("", &vim, &theme);
        let history_before = state.input_history.clone();
        attach_test_image(&mut state, &mut textarea);

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
            Ok(UserAction::SubmitWithMentions { prompt, images, .. })
                if prompt == "[Image #1]" && images.len() == 1
        ));
        assert!(textarea_text(&textarea).is_empty());
        assert!(state.composer_images.is_empty());
        assert_eq!(state.input_history, history_before);
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::Image(image)) if image.label == "[Image #1]"
        ));
    }

    #[test]
    fn non_vision_model_submits_images_for_runtime_analysis() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("inspect", &vim, &theme);
        attach_test_image(&mut state, &mut textarea);
        state.model_name = orca_core::model::PRO_MODEL.to_string();

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
            Ok(UserAction::SubmitWithMentions { prompt, images, .. })
                if prompt == "inspect [Image #1]" && images.len() == 1
        ));
        assert!(textarea_text(&textarea).is_empty());
        assert!(state.composer_images.is_empty());
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::Image(image)) if image.label == "[Image #1]"
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
            state.transcript.messages.last(),
            Some(ChatMessage::Diagnostic(diagnostic))
                if diagnostic.code() == "input.invalid"
                    && diagnostic.detail().contains("/workflow:<name>")
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
            state.transcript.messages.last(),
            Some(ChatMessage::Diagnostic(diagnostic))
                if diagnostic.code() == "input.invalid"
                    && diagnostic.detail().contains("unknown slash command")
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
            questionnaire: crate::protocol::TuiUserInputQuestionnaire::single(
                "Which path?",
                Vec::new(),
            ),
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
        assert!(state.interaction.pending_input.is_none());
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
            state.interaction.pending_input.as_ref(),
            Some(PendingTuiInput::McpElicitation(actual_key)) if actual_key == &key
        ));
        assert_eq!(textarea_text(&textarea), "not-json");
        assert!(matches!(
            state.transcript.messages.last(),
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
        assert!(state.interaction.pending_input.is_none());
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
                    questionnaire: crate::protocol::TuiUserInputQuestionnaire::single(
                        "Continue?",
                        Vec::new(),
                    ),
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
                state
                    .interaction
                    .pending_input
                    .as_ref()
                    .map(PendingTuiInput::key),
                Some(&key)
            );
            assert_eq!(
                state.interaction.pending_mcp_elicitation_mode,
                expected_mode
            );
            assert_eq!(textarea_text(&textarea), "");
        }
    }

    #[test]
    fn oversized_expanded_chat_preserves_composer_and_does_not_dispatch() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let placeholder = format!("[Pasted Content {} chars]", MAX_USER_INPUT_TEXT_CHARS + 1);
        state.pending_pastes.push((
            placeholder.clone(),
            "x".repeat(MAX_USER_INPUT_TEXT_CHARS + 1),
        ));
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text(&placeholder, &vim, &theme);

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
        assert_eq!(textarea_text(&textarea), placeholder);
        assert_eq!(state.pending_pastes.len(), 1);
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::Error(message))
                if message.contains("Message exceeds the maximum length")
        ));
    }

    /// The history of the conversation resumed at launch arrives: the
    /// reducer takes it in, and the renderer's runtime owner ends the hold
    /// for it.
    fn history_loads(state: &mut AppState) {
        state.update(crate::protocol::TuiEvent::HistoryLoaded {
            messages: Vec::new(),
            plan: None,
            label: "Resumed saved conversation.".to_string(),
        });
        state.startup_history_pending = false;
    }

    #[test]
    fn a_message_held_for_the_history_keeps_its_images() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            "/tmp".to_string(),
        );
        state.startup_history_pending = true;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("inspect", &vim, &theme);
        attach_test_image(&mut state, &mut textarea);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert!(action_rx.try_recv().is_err(), "nothing is sent yet");
        assert!(state.composer_images.is_empty(), "the composer is cleared");
        assert_eq!(
            state
                .held_submissions_preview()
                .map(|preview| preview.first),
            Some("inspect [Image #1]".to_string()),
            "it is listed above the composer"
        );

        // No prompt came with the conversation: the message starts the turn,
        // as it would have between turns.
        history_loads(&mut state);
        start_held_turn(&mut state, &action_tx);

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, images, .. })
                if prompt == "inspect [Image #1]" && images.len() == 1
        ));
        assert!(matches!(
            state.transcript.messages.as_slice(),
            [
                ChatMessage::System { .. },
                ChatMessage::User(text),
                ChatMessage::Image(image),
            ] if text == "inspect [Image #1]" && image.label == "[Image #1]"
        ));
        assert!(state.held_submissions.is_empty());
        assert!(state.startup_turn.is_some(), "its turn is not active yet");
    }

    /// A skill run with its `/skill-id` alias is held like a message typed
    /// then: sent at once, it went out before the command-line prompt and
    /// the history coming after washed it off the screen.
    #[test]
    fn a_skill_run_typed_before_the_history_loads_is_held() {
        let skill = format!("held-skill-{}", uuid::Uuid::new_v4().simple());
        let skill_dir = orca_core::home::orca_home()
            .expect("the test Orca home")
            .join("skills")
            .join(&skill);
        std::fs::create_dir_all(&skill_dir).expect("the skill's directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: held\ndescription: held before the history\n---\nDo it.\n",
        )
        .expect("the skill");
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.startup_history_pending = true;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text(&format!("/{skill}"), &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));

        assert!(action_rx.try_recv().is_err(), "nothing is sent yet");
        assert_eq!(
            state
                .held_submissions_preview()
                .map(|preview| preview.first),
            Some(format!("${skill}")),
            "it is listed above the composer"
        );

        history_loads(&mut state);
        start_held_turn(&mut state, &action_tx);

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == format!("${skill}")
        ));
        let _ = std::fs::remove_dir_all(skill_dir);
    }

    #[test]
    fn a_held_message_queued_behind_a_turn_sends_what_was_pasted_into_it() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.startup_history_pending = true;
        let placeholder = "[Pasted Content 1001 chars]";
        let payload = "secret payload\n".repeat(100);
        state.pending_pastes = vec![(placeholder.to_string(), payload.clone())];
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text(&format!("review {placeholder}"), &vim, &theme);

        assert!(handle_idle_submit(
            &mut textarea,
            &mut vim,
            &theme,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
        ));
        assert!(state.pending_pastes.is_empty(), "the composer is cleared");

        // The turn the message waited for is active: it queues behind it, with
        // the paste in it, as it would have when typed then.
        history_loads(&mut state);
        let token = state.new_submit_token();
        state.startup_turn = Some(token);
        release_held_submissions(&mut state, &action_tx, Some(token));

        let Ok(UserAction::QueuePrompt { prompt, .. }) = action_rx.try_recv() else {
            panic!("expected a queued prompt");
        };
        assert!(prompt.contains(payload.trim()));
        assert!(!prompt.contains(placeholder));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn a_message_held_while_a_turn_waits_is_queued_with_the_text_the_queue_would_send() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            "/tmp".to_string(),
        );
        let token = state.new_submit_token();
        state.startup_turn = Some(token);
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("inspect", &vim, &theme);
        attach_test_image(&mut state, &mut textarea);

        // Held as a plain message (the conversation is idle as far as the
        // composer goes), queued as a follow-up: an image label is not part
        // of what the runtime queue is given, as for any follow-up with one.
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
        release_held_submissions(&mut state, &action_tx, Some(token));

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::QueuePrompt { prompt, images, .. })
                if prompt == "inspect" && images.len() == 1
        ));
    }

    #[test]
    fn a_held_message_the_action_queue_cannot_take_is_named_not_lost_without_a_word() {
        let (action_tx, action_rx) = mpsc::bounded(1);
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let token = state.new_submit_token();
        state.startup_turn = Some(token);
        state.held_submissions.push(HeldSubmission::Plain {
            visible_text: "review the diff".to_string(),
            prompt: "review the diff".to_string(),
            bindings: MentionBindings::default(),
            images: Vec::new(),
            pending_pastes: Vec::new(),
        });
        action_tx
            .try_send(UserAction::Interrupt)
            .expect("fill the action queue");

        release_held_submissions(&mut state, &action_tx, Some(token));

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
        assert!(action_rx.try_recv().is_err(), "nothing else was sent");
        assert_eq!(
            state.queued_input_error(),
            Some("could not queue \"review the diff\"")
        );
        assert!(!state.holds_submissions());
    }
}
