//! What `/mcp__{server}__{prompt}` does once its arguments fit the prompt.
//! The MCP server expands the prompt, asked through the runtime of the
//! thread in view, or, before the first message, directly, as one of the
//! servers that started with the TUI. That can take up to the server's tool
//! timeout, so it runs on a worker of its own, off the UI thread and the
//! action dispatcher. The worker checks the expansion against what one
//! message can carry, attaches its images, and always reports back with
//! `McpPromptExpanded`. The UI thread sends the message only into the
//! conversation the prompt was run in, the way the composer sends one: at
//! once between turns, which starts the conversation's thread when it is
//! the first message, or queued while a turn runs. `Esc` while it expands
//! cancels it: the server's answer is then dropped.

use crossbeam_channel::Sender;
use orca_core::conversation::{ImageInput, ImageSource};
use orca_runtime::mentions::MentionBindings;
use orca_runtime::surface::SurfaceMcpPromptExpansion;

use crate::clipboard_image::{MAX_COMPOSER_IMAGE_BYTES, MAX_COMPOSER_IMAGE_COUNT};
use crate::commands::mcp_prompt_command;
use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::composer_textarea::MAX_USER_INPUT_TEXT_CHARS;
use crate::idle_submit_actions::submit_user_message;
use crate::prestart_mcp::McpServers;
use crate::protocol::{SessionAttachmentId, TuiEvent};
use crate::queued_input::QueuedUserMessage;
use crate::queued_input_actions::{FollowUp, dispatch_follow_up};
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus, PendingMcpPrompt};

/// A prompt's expansion as the composer holds a message (its text, then a
/// label for each attached image), or what the conversation says instead.
type McpPromptMessage = Result<(String, Vec<ComposerImageAttachment>), ChatMessage>;

/// Why there is no expansion when a worker ends without one.
const WORKER_STOPPED: &str = "its worker stopped unexpectedly";

/// Starts expanding the prompt `prompt` of the MCP server the catalog names
/// `server`, with `arguments` (name, value), for the conversation
/// `attachment`, as the run `token`, on `servers`, the MCP servers in view,
/// on a worker of its own. When no worker starts, the report it would have
/// sent comes back, for the caller to deliver.
pub(crate) fn spawn_mcp_prompt_expansion(
    server: String,
    prompt: String,
    arguments: Vec<(String, String)>,
    attachment: Option<SessionAttachmentId>,
    token: u64,
    servers: Option<McpServers>,
    event_tx: Sender<TuiEvent>,
) -> Result<(), Box<TuiEvent>> {
    let (failed_server, failed_prompt) = (server.clone(), prompt.clone());
    std::thread::Builder::new()
        .name("orca-tui-mcp-prompt".to_string())
        .spawn(move || {
            run_mcp_prompt_worker(
                server,
                prompt,
                attachment,
                token,
                &event_tx,
                |server, prompt| expand_prompt_on(servers, server, prompt, arguments),
            );
        })
        .map(|_| ())
        .map_err(|error| {
            let command = mcp_prompt_command(&failed_server, &failed_prompt);
            Box::new(TuiEvent::McpPromptExpanded {
                server: failed_server,
                prompt: failed_prompt,
                attachment,
                token,
                message: Err(failed(
                    &command,
                    &format!("could not start a worker: {error}"),
                )),
            })
        })
}

/// Has `server` (its catalog name), one of `servers`, expand its prompt
/// `prompt` with `arguments` (name, value).
fn expand_prompt_on(
    servers: Option<McpServers>,
    server: &str,
    prompt: &str,
    arguments: Vec<(String, String)>,
) -> Result<SurfaceMcpPromptExpansion, String> {
    match servers {
        Some(McpServers::Thread(runtime)) => crate::surface_client::expand_mcp_prompt(
            &runtime.typed_surface(),
            server,
            prompt,
            arguments,
        ),
        Some(McpServers::Prestarted(registry)) => registry
            .get_prompt(server, prompt, &arguments.into_iter().collect())
            .map(|expansion| SurfaceMcpPromptExpansion {
                text: expansion.text,
                images: expansion.images,
            }),
        // Neither a thread nor servers that started with the TUI, as while
        // a thread is taking them over.
        None => Err("the conversation is unavailable".to_string()),
    }
}

/// A worker's whole run: `expand` the prompt, then report the message it
/// makes, or why there is none, however `expand` ends, a panic included.
fn run_mcp_prompt_worker(
    server: String,
    prompt: String,
    attachment: Option<SessionAttachmentId>,
    token: u64,
    event_tx: &Sender<TuiEvent>,
    expand: impl FnOnce(&str, &str) -> Result<SurfaceMcpPromptExpansion, String>,
) {
    let command = mcp_prompt_command(&server, &prompt);
    let mut report = ReportOnDrop {
        event_tx: event_tx.clone(),
        server,
        prompt,
        attachment,
        token,
        message: Err(failed(&command, WORKER_STOPPED)),
    };
    report.message =
        prompt_message(&command, expand(&report.server, &report.prompt)).map_err(|say| *say);
}

/// Sends `McpPromptExpanded` with `message` when dropped, which reports on
/// every way out of a worker, unwinding included.
struct ReportOnDrop {
    event_tx: Sender<TuiEvent>,
    server: String,
    prompt: String,
    attachment: Option<SessionAttachmentId>,
    token: u64,
    message: McpPromptMessage,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        // A worker's own thread may wait for room in the event channel.
        let _ = self.event_tx.send(TuiEvent::McpPromptExpanded {
            server: std::mem::take(&mut self.server),
            prompt: std::mem::take(&mut self.prompt),
            attachment: self.attachment,
            token: self.token,
            message: std::mem::replace(&mut self.message, Ok(Default::default())),
        });
    }
}

/// What the conversation says when the prompt `command` ran could not be
/// expanded, `reason` being why.
fn failed(command: &str, reason: &str) -> ChatMessage {
    ChatMessage::Error(format!("MCP prompt {command} failed: {reason}"))
}

/// The message `command` expanded to, as `result` has it. Nothing is sent
/// when the server could not expand the prompt, or when the expansion is
/// empty or more than one message can carry; `Err` is what the
/// conversation says instead.
fn prompt_message(
    command: &str,
    result: Result<SurfaceMcpPromptExpansion, String>,
) -> Result<(String, Vec<ComposerImageAttachment>), Box<ChatMessage>> {
    let expansion = result.map_err(|reason| Box::new(failed(command, &reason)))?;
    let text = expansion.text.trim();
    if text.is_empty() && expansion.images.is_empty() {
        return Err(Box::new(ChatMessage::System {
            text: format!("MCP prompt {command} returned nothing to send"),
            expanded: false,
        }));
    }
    if let Err(reason) = check_message_limits(text, &expansion.images) {
        return Err(Box::new(ChatMessage::Error(format!(
            "MCP prompt {command} is too large to send: {reason}"
        ))));
    }
    let (text, images) = ComposerImageState::attach_inputs(text, expansion.images);
    Ok((text.trim().to_string(), images))
}

/// Why `text` and `images` are more than one message can carry. Images are
/// measured by their base64 text, before any is decoded; their total bounds
/// each image too.
fn check_message_limits(text: &str, images: &[ImageInput]) -> Result<(), String> {
    let chars = text.chars().count();
    if chars > MAX_USER_INPUT_TEXT_CHARS {
        return Err(format!(
            "its text exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters ({chars} provided)"
        ));
    }
    if images.len() > MAX_COMPOSER_IMAGE_COUNT {
        return Err(format!(
            "its {} images exceed Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit",
            images.len()
        ));
    }
    let inline_bytes = images.iter().fold(0usize, |total, image| {
        total.saturating_add(inline_bytes(image))
    });
    if inline_bytes > MAX_COMPOSER_IMAGE_BYTES {
        return Err(format!(
            "its images exceed Orca's {} MiB inline limit",
            MAX_COMPOSER_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// The bytes `image` carries inline once decoded, counted from its base64
/// text without decoding it. An image by url or file id carries none.
fn inline_bytes(image: &ImageInput) -> usize {
    match &image.source {
        ImageSource::Base64 { data, .. } => {
            let digits = data.trim_end_matches('=').len();
            digits / 4 * 3 + digits % 4 * 3 / 4
        }
        ImageSource::Url { .. } | ImageSource::File { .. } => 0,
    }
}

impl AppState {
    /// Notes that the MCP prompt `command` (`/mcp__{server}__{prompt}`) runs
    /// in the conversation in view, until its expansion comes back, and
    /// returns the token its run and its expansion carry.
    pub(crate) fn start_mcp_prompt(&mut self, command: String) -> u64 {
        let token = self.next_mcp_prompt_token;
        self.next_mcp_prompt_token += 1;
        self.pending_mcp_prompts.push(PendingMcpPrompt {
            token,
            attachment: self.active_session_attachment,
            command,
            cancelled: false,
        });
        token
    }

    /// `Esc` while an MCP prompt run in the conversation in view is still
    /// expanding: cancels the latest such, and says so. What the server
    /// makes of it is dropped when it comes; its request is left to finish.
    /// `false` when no prompt is expanding there.
    pub(crate) fn cancel_running_mcp_prompt(&mut self) -> bool {
        let attachment = self.active_session_attachment;
        let Some(pending) = self
            .pending_mcp_prompts
            .iter_mut()
            .rev()
            .find(|pending| !pending.cancelled && pending.attachment == attachment)
        else {
            return false;
        };
        pending.cancelled = true;
        let text = format!("MCP prompt {} cancelled", pending.command);
        self.push_message(ChatMessage::System {
            text,
            expanded: false,
        });
        true
    }

    /// The expansion of the run `token` has come back: whether `Esc`
    /// cancelled that run.
    fn finish_mcp_prompt(&mut self, token: u64) -> bool {
        let Some(index) = self
            .pending_mcp_prompts
            .iter()
            .position(|pending| pending.token == token)
        else {
            return false;
        };
        self.pending_mcp_prompts.remove(index).cancelled
    }

    /// What the worker for `/mcp__{server}__{prompt}`, run in the
    /// conversation `attachment` as the run `token`, reported: `message` is
    /// sent as the user's, or the conversation says why it is not. Only the
    /// conversation the prompt was run in gets it; after `/new`, a fork, a
    /// resume, or a switch to a side or child conversation, and while the
    /// session picker or setup is open, nothing is sent. Nothing at all is
    /// done with a run `Esc` cancelled.
    pub(crate) fn submit_mcp_prompt_expansion(
        &mut self,
        server: &str,
        prompt: &str,
        attachment: Option<SessionAttachmentId>,
        token: u64,
        message: McpPromptMessage,
    ) {
        if self.finish_mcp_prompt(token) {
            return;
        }
        let command = mcp_prompt_command(server, prompt);
        if attachment != self.active_session_attachment
            || matches!(self.status, AppStatus::SessionPicker | AppStatus::Setup)
        {
            return self.say_mcp_prompt_not_sent(ChatMessage::System {
                text: format!("MCP prompt {command} was not sent: the conversation changed"),
                expanded: false,
            });
        }
        match message {
            Ok((text, images)) => self.send_mcp_prompt_message(&command, text, images),
            Err(not_sent) => self.say_mcp_prompt_not_sent(not_sent),
        }
    }

    /// Sends `text`, with `images` attached, as the user's message, and as
    /// literal text: an `@` path in it is not a mention for Orca to read.
    /// It goes the composer's way: at once between turns, and queued while
    /// a turn runs.
    fn send_mcp_prompt_message(
        &mut self,
        command: &str,
        text: String,
        images: Vec<ComposerImageAttachment>,
    ) {
        let action_tx = self.event_tx.clone();
        let bindings = MentionBindings::new(&text);
        if self.status == AppStatus::Idle {
            return submit_user_message(self, &action_tx, text.clone(), text, bindings, images);
        }
        let message =
            QueuedUserMessage::from_composer_with_images(text, Vec::new(), bindings, images);
        debug_assert!(
            message.is_some(),
            "an MCP prompt message has text or an image label"
        );
        let not_sent = match message {
            Some(message) => {
                if dispatch_follow_up(self, &action_tx, message, FollowUp::Queue) {
                    return;
                }
                "follow-up action queue is unavailable"
            }
            None => "it has nothing to send",
        };
        self.say_mcp_prompt_not_sent(ChatMessage::Error(format!(
            "MCP prompt {command} was not sent: {not_sent}"
        )));
    }

    /// Says `message`, about a prompt whose message is not sent, after any
    /// assistant text still streaming in.
    fn say_mcp_prompt_not_sent(&mut self, message: ChatMessage) {
        self.finish_assistant_stream();
        self.push_message(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use orca_core::conversation::ImageDetail;

    use crate::protocol::{AttachedTuiEvent, UserAction};
    use crate::slash_command_actions::handle_slash_command;
    use crate::surface_projection::{
        McpCatalogView, McpPromptView, McpServerStatusView, McpServerView,
    };

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    /// The conversation in view in `prompt_state`.
    const ATTACHMENT: SessionAttachmentId = SessionAttachmentId::new(1);

    fn png() -> ImageInput {
        orca_core::tool_images::tool_image("image/png", PNG.to_string()).expect("a valid PNG")
    }

    /// An image of `bytes` bytes, whatever they show.
    fn image_of(bytes: usize) -> ImageInput {
        ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(vec![0u8; bytes]),
            },
            detail: ImageDetail::High,
        }
    }

    /// A conversation whose runtime has started, with the MCP server
    /// `github` and its prompt `review_pr <pr> [branch]`.
    fn prompt_state() -> (AppState, crossbeam_channel::Receiver<UserAction>) {
        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        (prompt_state_with(action_tx), action_rx)
    }

    /// `prompt_state`, sending its actions to `action_tx`.
    fn prompt_state_with(action_tx: crossbeam_channel::Sender<UserAction>) -> AppState {
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.active_session_attachment = Some(ATTACHMENT);
        state.mcp_catalog = McpCatalogView {
            servers: vec![McpServerView {
                name: "github".to_string(),
                status: McpServerStatusView::Connected,
                prompts_error: None,
            }],
            tools: Vec::new(),
            prompts: vec![McpPromptView {
                server: "github".to_string(),
                name: "review_pr".to_string(),
                description: Some("Review a pull request".to_string()),
                arguments: vec![("pr".to_string(), true), ("branch".to_string(), false)],
            }],
        };
        state
    }

    /// What a worker for `/mcp__github__review_pr`, run in the conversation
    /// `prompt_state` shows, reports when the server answers with `result`.
    fn report(result: Result<SurfaceMcpPromptExpansion, String>) -> TuiEvent {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        run_mcp_prompt_worker(
            "github".to_string(),
            "review_pr".to_string(),
            Some(ATTACHMENT),
            1,
            &event_tx,
            |_, _| result,
        );
        event_rx.try_recv().expect("the worker's report")
    }

    /// The report for an expansion to `text` and `images`.
    fn expanded(text: &str, images: Vec<ImageInput>) -> TuiEvent {
        report(Ok(SurfaceMcpPromptExpansion {
            text: text.to_string(),
            images,
        }))
    }

    fn last_message(state: &AppState) -> Option<&ChatMessage> {
        state.transcript.messages.last()
    }

    #[test]
    fn running_a_prompt_submits_its_expansion() {
        let (mut state, action_rx) = prompt_state();
        let action_tx = state.event_tx.clone();
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));

        handle_slash_command(
            "/mcp__github__review_pr 123 against main",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );

        let Ok(UserAction::RunMcpPrompt {
            server,
            prompt,
            arguments,
            attachment,
            token,
        }) = action_rx.try_recv()
        else {
            panic!("the prompt did not run");
        };
        assert_eq!((server.as_str(), prompt.as_str()), ("github", "review_pr"));
        assert_eq!(
            arguments,
            [
                ("pr".to_string(), "123".to_string()),
                ("branch".to_string(), "against main".to_string()),
            ]
        );
        assert_eq!(attachment, Some(ATTACHMENT));
        assert!(matches!(
            last_message(&state),
            Some(ChatMessage::System { text, .. })
                if text == "running MCP prompt /mcp__github__review_pr…"
        ));
        // ↑ brings the command back, as it does a skill's.
        assert_eq!(
            state.input_history.last().map(String::as_str),
            Some("/mcp__github__review_pr 123 against main")
        );
        assert_eq!(state.status, AppStatus::Idle);

        // The worker's report, from the action's own run.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        run_mcp_prompt_worker(server, prompt, attachment, token, &event_tx, |_, _| {
            Ok(SurfaceMcpPromptExpansion {
                text: "Review pull request 123 against main.".to_string(),
                images: vec![png()],
            })
        });
        state.update(event_rx.try_recv().expect("the worker's report"));

        let Ok(UserAction::SubmitWithMentions {
            prompt,
            bindings,
            images,
        }) = action_rx.try_recv()
        else {
            panic!("the expansion was not sent");
        };
        assert_eq!(prompt, "Review pull request 123 against main. [Image #1]");
        assert!(bindings.is_empty());
        assert_eq!(ComposerImageState::image_inputs(&images), [png()]);
        // The model gets the server's text as it is, and the image with it.
        assert_eq!(
            ComposerImageState::submission_text_and_bindings(&prompt, &images, &bindings).0,
            "Review pull request 123 against main."
        );
        assert_eq!(state.status, AppStatus::Running);
        let shown = &state.transcript.messages[state.transcript.messages.len() - 2..];
        assert!(
            matches!(
                shown,
                [ChatMessage::User(text), ChatMessage::Image(_)]
                    if text == "Review pull request 123 against main. [Image #1]"
            ),
            "{shown:?}"
        );
    }

    #[test]
    fn a_prompt_typed_with_a_paste_gets_the_pasted_text() {
        use orca_core::config::ThemeName;

        use crate::composer_textarea::{make_textarea_with_text, textarea_text};
        use crate::theme::Theme;
        use crate::vim::VimState;

        let (mut state, action_rx) = prompt_state();
        let action_tx = state.event_tx.clone();
        let mut config = crate::test_support::test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let placeholder = "[Pasted Content 1001 chars]";
        let pasted = format!("rebase onto main\n{}", "x".repeat(984));
        state.pending_pastes = vec![(placeholder.to_string(), pasted.clone())];
        let mut textarea = make_textarea_with_text(
            &format!("/mcp__github__review_pr 123 {placeholder}"),
            &vim,
            &theme,
        );

        assert!(crate::idle_submit_actions::handle_idle_submit(
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
            Ok(UserAction::RunMcpPrompt { arguments, .. })
                if arguments == [
                    ("pr".to_string(), "123".to_string()),
                    ("branch".to_string(), pasted.clone()),
                ]
        ));
        assert_eq!(textarea_text(&textarea), "");
        assert!(state.pending_pastes.is_empty());
        assert_eq!(
            state.input_history.last(),
            Some(&format!("/mcp__github__review_pr 123 {pasted}"))
        );
    }

    #[test]
    fn a_prompt_that_expands_while_a_turn_runs_is_queued() {
        for status in [
            AppStatus::Running,
            AppStatus::WaitingApproval,
            AppStatus::WaitingUserInput,
            AppStatus::Compacting,
        ] {
            let (mut state, action_rx) = prompt_state();
            state.set_status(status);
            let shown = state.transcript.messages.len();

            // The server's `@` path is text, not a file for Orca to read,
            // and its image label is text too.
            state.update(expanded(
                "Compare @src/main.rs with [Image #1]",
                vec![png()],
            ));

            let Ok(UserAction::QueuePrompt {
                prompt,
                bindings,
                images,
            }) = action_rx.try_recv()
            else {
                panic!("{status:?}: the expansion was not queued");
            };
            assert_eq!(prompt, "Compare @src/main.rs with [Image #1]");
            assert!(bindings.is_empty());
            assert_eq!(ComposerImageState::image_inputs(&images), [png()]);
            // Like any input queued during a turn, it joins the
            // conversation when its own turn starts.
            assert_eq!(state.transcript.messages.len(), shown, "{status:?}");
            assert_eq!(state.status, status);
        }
    }

    #[test]
    fn a_queued_expansion_the_action_queue_cannot_take_is_not_sent() {
        let (action_tx, action_rx) = crossbeam_channel::bounded(1);
        let mut state = prompt_state_with(action_tx.clone());
        state.enter_running();
        action_tx
            .try_send(UserAction::Interrupt)
            .expect("fill the action queue");

        state.update(expanded("Review pull request 123.", Vec::new()));

        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            last_message(&state),
            Some(ChatMessage::Error(text)) if text
                == "MCP prompt /mcp__github__review_pr was not sent: follow-up action queue is unavailable"
        ));
    }

    #[test]
    fn an_expansion_for_a_conversation_no_longer_in_view_is_not_sent() {
        let (mut state, action_rx) = prompt_state();
        // `/new`, a fork, a resume or a side conversation gives the view a
        // new attachment.
        crate::attachment_routing::accept_attached_tui_event(
            &mut state,
            TuiEvent::Attached(Box::new(AttachedTuiEvent {
                attachment: Some(SessionAttachmentId::new(2)),
                event: TuiEvent::SessionAttachmentActivated,
            })),
        )
        .expect("activate the new attachment");

        state.update(expanded("Review pull request 123.", vec![png()]));

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::Idle);
        assert!(matches!(
            last_message(&state),
            Some(ChatMessage::System { text, .. })
                if text == "MCP prompt /mcp__github__review_pr was not sent: the conversation changed"
        ));
    }

    #[test]
    fn an_expansion_is_not_sent_while_the_session_picker_or_setup_is_open() {
        for status in [AppStatus::SessionPicker, AppStatus::Setup] {
            let (mut state, action_rx) = prompt_state();
            state.set_status(status);

            state.update(expanded("Review pull request 123.", Vec::new()));

            assert!(action_rx.try_recv().is_err(), "{status:?}");
            assert_eq!(state.status, status);
            assert!(
                matches!(
                    last_message(&state),
                    Some(ChatMessage::System { text, .. })
                        if text == "MCP prompt /mcp__github__review_pr was not sent: the conversation changed"
                ),
                "{status:?}"
            );
        }
    }

    #[test]
    fn an_expansion_too_large_to_send_sends_nothing() {
        let five_mib = 5 * 1024 * 1024;
        for ((text, images), reason) in [
            (
                ("x".repeat(MAX_USER_INPUT_TEXT_CHARS + 1), Vec::new()),
                format!(
                    "its text exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters ({} provided)",
                    MAX_USER_INPUT_TEXT_CHARS + 1
                ),
            ),
            (
                (String::new(), vec![png(); MAX_COMPOSER_IMAGE_COUNT + 1]),
                format!(
                    "its {} images exceed Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit",
                    MAX_COMPOSER_IMAGE_COUNT + 1
                ),
            ),
            (
                ("one screenshot".to_string(), vec![image_of(five_mib + 1)]),
                "its images exceed Orca's 5 MiB inline limit".to_string(),
            ),
            (
                (
                    "two screenshots".to_string(),
                    vec![image_of(five_mib / 2), image_of(five_mib / 2 + 1)],
                ),
                "its images exceed Orca's 5 MiB inline limit".to_string(),
            ),
        ] {
            let (mut state, action_rx) = prompt_state();

            state.update(expanded(&text, images));

            assert!(action_rx.try_recv().is_err(), "{reason}");
            assert_eq!(state.status, AppStatus::Idle);
            assert!(
                matches!(
                    last_message(&state),
                    Some(ChatMessage::Error(message)) if *message == format!(
                        "MCP prompt /mcp__github__review_pr is too large to send: {reason}"
                    )
                ),
                "{reason}: {:?}",
                last_message(&state)
            );
        }

        // Right at the limits, it is sent.
        let (mut state, action_rx) = prompt_state();
        state.update(expanded(
            &"x".repeat(MAX_USER_INPUT_TEXT_CHARS),
            vec![image_of(five_mib / 2), image_of(five_mib / 2)],
        ));
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWithMentions { .. })
        ));
    }

    #[test]
    fn image_bytes_are_counted_from_base64_without_decoding() {
        for bytes in [0, 1, 2, 3, 4, 5, 6, 1023, 5 * 1024 * 1024 + 1] {
            assert_eq!(inline_bytes(&image_of(bytes)), bytes, "{bytes} bytes");
        }
        // Unpadded base64 counts the same.
        let unpadded = ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8; 5]),
            },
            detail: ImageDetail::High,
        };
        assert_eq!(inline_bytes(&unpadded), 5);
    }

    #[test]
    fn an_expansion_with_nothing_in_it_sends_nothing() {
        for text in ["", " \n\t "] {
            let (mut state, action_rx) = prompt_state();

            state.update(expanded(text, Vec::new()));

            assert!(action_rx.try_recv().is_err());
            assert_eq!(state.status, AppStatus::Idle);
            assert!(matches!(
                last_message(&state),
                Some(ChatMessage::System { text, .. })
                    if text == "MCP prompt /mcp__github__review_pr returned nothing to send"
            ));
        }
    }

    #[test]
    fn a_prompt_its_server_cannot_expand_says_why_and_sends_nothing() {
        let (mut state, action_rx) = prompt_state();

        state.update(report(Err(
            "MCP server 'github' has no prompt named 'review_pr'".to_string(),
        )));

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::Idle);
        assert!(matches!(
            last_message(&state),
            Some(ChatMessage::Error(text)) if text
                == "MCP prompt /mcp__github__review_pr failed: MCP server 'github' has no prompt named 'review_pr'"
        ));
    }

    #[test]
    fn a_prompt_worker_that_panics_still_reports_back() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let worker = std::thread::spawn(move || {
            run_mcp_prompt_worker(
                "github".to_string(),
                "review_pr".to_string(),
                Some(ATTACHMENT),
                1,
                &event_tx,
                |_, _| -> Result<SurfaceMcpPromptExpansion, String> {
                    panic!("the MCP prompt worker panicked")
                },
            )
        });

        assert!(worker.join().is_err(), "the worker did not panic");
        let (mut state, action_rx) = prompt_state();
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{events:?}");
        for event in events {
            state.update(event);
        }
        assert!(action_rx.try_recv().is_err());
        assert!(matches!(
            last_message(&state),
            Some(ChatMessage::Error(text))
                if text == "MCP prompt /mcp__github__review_pr failed: its worker stopped unexpectedly"
        ));
    }

    /// A stdio MCP server named `name` that offers the prompts in `prompts`
    /// and answers every `prompts/get` with `get_result`. Each message it
    /// gets is added to `<dir>/requests`.
    #[cfg(unix)]
    fn prompt_mcp_server(
        name: &str,
        dir: &std::path::Path,
        prompts: &str,
        get_result: &str,
    ) -> orca_core::mcp_types::McpServerConfig {
        std::fs::write(dir.join("prompts.json"), prompts).unwrap();
        std::fs::write(dir.join("get.json"), get_result).unwrap();
        let script = dir.join("server.sh");
        std::fs::write(
            &script,
            r#"state_dir="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"prompts":{}},"serverInfo":{"name":"prompts","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
      ;;
    *'"method":"prompts/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"prompts":%s}}\n' "$id" "$(cat "$state_dir/prompts.json")"
      ;;
    *'"method":"prompts/get"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$(cat "$state_dir/get.json")"
      ;;
  esac
done
"#,
        )
        .unwrap();
        orca_core::mcp_types::McpServerConfig {
            name: name.to_string(),
            command: Some("/bin/sh".to_string()),
            args: vec![
                script.to_string_lossy().into_owned(),
                dir.to_string_lossy().into_owned(),
            ],
            startup_timeout_ms: Some(15_000),
            tool_timeout_ms: Some(15_000),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_runs_through_the_thread_s_runtime() {
        use std::time::{Duration, Instant};

        use crate::surface_projection::SurfaceProjectionState;

        let home = crate::test_support::isolate_orca_home();
        let fixture = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = orca_core::config::HistoryMode::Record;
        let expansion = serde_json::json!({"messages": [
            {"role": "user", "content": {"type": "text", "text": "Review pull request 123 against main."}},
            {"role": "user", "content": {"type": "image", "data": PNG, "mimeType": "image/png"}}
        ]});
        // The catalog names the server `docs`, and so does its command.
        config.mcp_servers = vec![prompt_mcp_server(
            "Docs",
            fixture.path(),
            r#"[{"name":"review_pr","arguments":[{"name":"pr","required":true},{"name":"branch"}]}]"#,
            &expansion.to_string(),
        )];
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "mcp prompt")
            .expect("runtime thread");
        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        // What the controller's idle poll does with each new cursor.
        let deadline = Instant::now() + Duration::from_secs(15);
        while state.mcp_catalog.prompts.is_empty() {
            assert!(Instant::now() < deadline, "no prompts in the catalog");
            let snapshot = crate::surface_client::read_snapshot(&thread.typed_surface())
                .expect("thread snapshot");
            state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
                SurfaceProjectionState::from_surface_snapshot(&snapshot),
            )));
            std::thread::sleep(Duration::from_millis(10));
        }
        let shared = Arc::new(Mutex::new(config.clone()));

        handle_slash_command(
            "/mcp__docs__review_pr 123 main",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        let Ok(UserAction::RunMcpPrompt {
            server,
            prompt,
            arguments,
            attachment,
            token,
        }) = action_rx.try_recv()
        else {
            panic!("the prompt did not run");
        };
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        spawn_mcp_prompt_expansion(
            server,
            prompt,
            arguments,
            attachment,
            token,
            Some(McpServers::Thread(Box::new(thread.clone()))),
            event_tx,
        )
        .expect("start the MCP prompt worker");
        state.update(
            event_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("the expansion"),
        );

        let Ok(UserAction::SubmitWithMentions { prompt, images, .. }) = action_rx.try_recv() else {
            panic!("nothing was sent: {:?}", state.transcript.messages.last());
        };
        assert_eq!(prompt, "Review pull request 123 against main. [Image #1]");
        assert_eq!(ComposerImageState::image_inputs(&images), [png()]);
        let asked = std::fs::read_to_string(fixture.path().join("requests"))
            .expect("read the MCP request log")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON-RPC"))
            .filter(|message| message["method"] == "prompts/get")
            .collect::<Vec<_>>();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(
            asked[0]["params"],
            serde_json::json!({"name": "review_pr", "arguments": {"pr": "123", "branch": "main"}})
        );
        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    }
}
