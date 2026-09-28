//! What `/mcp__{server}__{prompt}` does once its arguments fit the prompt.
//! The runtime of the thread in view has the MCP server expand the prompt,
//! which can take up to the server's tool timeout, so it runs on a worker of
//! its own, off the UI thread and the action dispatcher. The worker always
//! reports back with `McpPromptExpanded`, and the UI thread sends the
//! expansion as the user's message, as the composer sends one: at once
//! between turns, or queued while a turn runs.

use crossbeam_channel::Sender;
use orca_runtime::mentions::MentionBindings;
use orca_runtime::runtime_host::RuntimeThreadHandle;
use orca_runtime::surface::SurfaceMcpPromptExpansion;

use crate::clipboard_image::{MAX_COMPOSER_IMAGE_BYTES, MAX_COMPOSER_IMAGE_COUNT};
use crate::commands::mcp_prompt_command;
use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::composer_textarea::MAX_USER_INPUT_TEXT_CHARS;
use crate::protocol::{TuiEvent, UserAction};
use crate::queued_input::QueuedUserMessage;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus};

/// Why there is no expansion when a worker ends without one.
const WORKER_STOPPED: &str = "its worker stopped unexpectedly";

/// Starts expanding the prompt `prompt` of the MCP server the catalog names
/// `server`, with `arguments` (name, value), on the MCP servers of
/// `runtime`, the thread in view, on a worker of its own. When no worker
/// starts, the report it would have sent comes back, for the caller to
/// deliver.
pub(crate) fn spawn_mcp_prompt_expansion(
    server: String,
    prompt: String,
    arguments: Vec<(String, String)>,
    runtime: Option<RuntimeThreadHandle>,
    event_tx: Sender<TuiEvent>,
) -> Result<(), Box<TuiEvent>> {
    let (failed_server, failed_prompt) = (server.clone(), prompt.clone());
    std::thread::Builder::new()
        .name("orca-tui-mcp-prompt".to_string())
        .spawn(move || {
            run_mcp_prompt_worker(server, prompt, &event_tx, |server, prompt| match runtime {
                Some(runtime) => crate::surface_client::expand_mcp_prompt(
                    &runtime.typed_surface(),
                    server,
                    prompt,
                    arguments,
                ),
                None => Err("the conversation has not started".to_string()),
            });
        })
        .map(|_| ())
        .map_err(|error| {
            Box::new(TuiEvent::McpPromptExpanded {
                server: failed_server,
                prompt: failed_prompt,
                result: Err(format!("could not start a worker: {error}")),
            })
        })
}

/// A worker's whole run: `expand` the prompt, then report the expansion, or
/// why there is none, however `expand` ends, a panic included.
fn run_mcp_prompt_worker(
    server: String,
    prompt: String,
    event_tx: &Sender<TuiEvent>,
    expand: impl FnOnce(&str, &str) -> Result<SurfaceMcpPromptExpansion, String>,
) {
    let mut report = ReportOnDrop {
        event_tx: event_tx.clone(),
        server,
        prompt,
        result: Err(WORKER_STOPPED.to_string()),
    };
    report.result = expand(&report.server, &report.prompt);
}

/// Sends `McpPromptExpanded` with `result` when dropped, which reports on
/// every way out of a worker, unwinding included.
struct ReportOnDrop {
    event_tx: Sender<TuiEvent>,
    server: String,
    prompt: String,
    result: Result<SurfaceMcpPromptExpansion, String>,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        // A worker's own thread may wait for room in the event channel.
        let _ = self.event_tx.send(TuiEvent::McpPromptExpanded {
            server: std::mem::take(&mut self.server),
            prompt: std::mem::take(&mut self.prompt),
            result: std::mem::replace(&mut self.result, Err(String::new())),
        });
    }
}

impl AppState {
    /// What the worker for `/mcp__{server}__{prompt}` brought back. An
    /// expansion is sent as the user's message; nothing is sent when the
    /// server could not expand the prompt, or when the expansion is empty
    /// or too large for Orca to send, and the conversation says why.
    pub(crate) fn submit_mcp_prompt_expansion(
        &mut self,
        server: &str,
        prompt: &str,
        result: Result<SurfaceMcpPromptExpansion, String>,
    ) {
        let command = mcp_prompt_command(server, prompt);
        let not_sent = match result {
            Err(reason) => ChatMessage::Error(format!("MCP prompt {command} failed: {reason}")),
            Ok(expansion) if expansion.text.trim().is_empty() && expansion.images.is_empty() => {
                ChatMessage::System {
                    text: format!("MCP prompt {command} returned nothing to send"),
                    expanded: false,
                }
            }
            Ok(expansion) => match composer_message(expansion) {
                Ok((text, images)) => return self.send_mcp_prompt_message(text, images),
                Err(reason) => ChatMessage::Error(format!(
                    "MCP prompt {command} is too large to send: {reason}"
                )),
            },
        };
        self.finish_assistant_stream();
        self.push_message(not_sent);
    }

    /// Sends `text`, with `images` attached, as the user's message, and as
    /// literal text: an `@` path in it is not a mention for Orca to read.
    /// Between turns it is sent at once; while a turn runs it is queued, as
    /// the composer queues a message then.
    fn send_mcp_prompt_message(&mut self, text: String, images: Vec<ComposerImageAttachment>) {
        let bindings = MentionBindings::new(&text);
        if self.status == AppStatus::Idle {
            self.push_user_message_with_images(text.clone(), &images);
            self.enter_running();
            self.scroll_to_bottom();
            let _ = self.event_tx.send(UserAction::SubmitWithMentions {
                prompt: text,
                bindings,
                images,
            });
            self.request_runtime_queue_start();
            self.resume_queued_follow_up_autosend();
            return;
        }
        let Some(message) =
            QueuedUserMessage::from_composer_with_images(text, Vec::new(), bindings, images)
        else {
            return;
        };
        let queued = UserAction::QueuePrompt {
            prompt: message.submission_text().to_string(),
            bindings: message.submission_bindings().clone(),
            images: message.images().to_vec(),
        };
        if self.event_tx.try_send(queued).is_err() {
            self.report_queued_input_error("follow-up action queue is unavailable".to_string());
            return;
        }
        self.remember_runtime_queued_message(message);
    }
}

/// `expansion` as the composer would hold it: its text, then a label for
/// each image, which is attached. `Err` says why Orca does not send it:
/// more than one message can carry.
fn composer_message(
    expansion: SurfaceMcpPromptExpansion,
) -> Result<(String, Vec<ComposerImageAttachment>), String> {
    let text = expansion.text.trim();
    let chars = text.chars().count();
    if chars > MAX_USER_INPUT_TEXT_CHARS {
        return Err(format!(
            "its text exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters ({chars} provided)"
        ));
    }
    if expansion.images.len() > MAX_COMPOSER_IMAGE_COUNT {
        return Err(format!(
            "its {} images exceed Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit",
            expansion.images.len()
        ));
    }
    let (text, images) = ComposerImageState::attach_inputs(text, expansion.images);
    // The total bounds each image too.
    if ComposerImageState::inline_bytes(&images) > MAX_COMPOSER_IMAGE_BYTES {
        return Err(format!(
            "its images exceed Orca's {} MiB inline limit",
            MAX_COMPOSER_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok((text.trim().to_string(), images))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use orca_core::conversation::{ImageDetail, ImageInput, ImageSource};

    use crate::clipboard_image::MAX_COMPOSER_IMAGE_COUNT;
    use crate::composer_images::ComposerImageState;
    use crate::composer_textarea::MAX_USER_INPUT_TEXT_CHARS;
    use crate::protocol::UserAction;
    use crate::slash_command_actions::handle_slash_command;
    use crate::surface_projection::{
        McpCatalogView, McpPromptView, McpServerStatusView, McpServerView,
    };
    use crate::transcript_state::ChatMessage;
    use crate::types::AppStatus;

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

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
        let mut state = AppState::new(
            action_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.mcp_catalog = McpCatalogView {
            servers: vec![McpServerView {
                name: "github".to_string(),
                status: McpServerStatusView::Connected,
            }],
            tools: Vec::new(),
            prompts: vec![McpPromptView {
                server: "github".to_string(),
                name: "review_pr".to_string(),
                description: Some("Review a pull request".to_string()),
                arguments: vec![("pr".to_string(), true), ("branch".to_string(), false)],
            }],
        };
        (state, action_rx)
    }

    /// What the worker brings back for `/mcp__github__review_pr`.
    fn expanded(text: &str, images: Vec<ImageInput>) -> TuiEvent {
        TuiEvent::McpPromptExpanded {
            server: "github".to_string(),
            prompt: "review_pr".to_string(),
            result: Ok(SurfaceMcpPromptExpansion {
                text: text.to_string(),
                images,
            }),
        }
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
        assert!(matches!(
            state.transcript.messages.last(),
            Some(ChatMessage::System { text, .. })
                if text == "running MCP prompt /mcp__github__review_pr…"
        ));
        assert_eq!(state.status, AppStatus::Idle);

        state.update(TuiEvent::McpPromptExpanded {
            server,
            prompt,
            result: Ok(SurfaceMcpPromptExpansion {
                text: "Review pull request 123 against main.".to_string(),
                images: vec![png()],
            }),
        });

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
    fn a_prompt_that_expands_while_a_turn_runs_is_queued() {
        let (mut state, action_rx) = prompt_state();
        state.enter_running();
        let shown = state.transcript.messages.len();

        // The server's `@` path is text, not a file for Orca to read, and
        // its image label is text too.
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
            panic!("the expansion was not queued");
        };
        assert_eq!(prompt, "Compare @src/main.rs with [Image #1]");
        assert!(bindings.is_empty());
        assert_eq!(ComposerImageState::image_inputs(&images), [png()]);
        // Like any input queued during a turn, it joins the conversation
        // when its own turn starts.
        assert_eq!(state.transcript.messages.len(), shown);
        assert_eq!(state.status, AppStatus::Running);
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
                    state.transcript.messages.last(),
                    Some(ChatMessage::Error(message)) if *message == format!(
                        "MCP prompt /mcp__github__review_pr is too large to send: {reason}"
                    )
                ),
                "{reason}: {:?}",
                state.transcript.messages.last()
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
    fn an_expansion_with_nothing_in_it_sends_nothing() {
        for text in ["", " \n\t "] {
            let (mut state, action_rx) = prompt_state();

            state.update(expanded(text, Vec::new()));

            assert!(action_rx.try_recv().is_err());
            assert_eq!(state.status, AppStatus::Idle);
            assert!(matches!(
                state.transcript.messages.last(),
                Some(ChatMessage::System { text, .. })
                    if text == "MCP prompt /mcp__github__review_pr returned nothing to send"
            ));
        }
    }

    #[test]
    fn a_prompt_its_server_cannot_expand_says_why_and_sends_nothing() {
        let (mut state, action_rx) = prompt_state();

        state.update(TuiEvent::McpPromptExpanded {
            server: "github".to_string(),
            prompt: "review_pr".to_string(),
            result: Err("MCP server 'github' has no prompt named 'review_pr'".to_string()),
        });

        assert!(action_rx.try_recv().is_err());
        assert_eq!(state.status, AppStatus::Idle);
        assert!(matches!(
            state.transcript.messages.last(),
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
            state.transcript.messages.last(),
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
        }) = action_rx.try_recv()
        else {
            panic!("the prompt did not run");
        };
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        spawn_mcp_prompt_expansion(server, prompt, arguments, Some(thread.clone()), event_tx)
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
