//! `orca` in a test, before its first message and after: the hosted
//! controller and its runtime as `orca` runs them, and the renderer's state,
//! shown each event as `orca` shows it. With it, stdio MCP servers that are
//! shell scripts, and say when they start.

// The shell-script servers run on unix only, and so do most of the tests
// that use the rest.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use orca_core::config::RunConfig;
use orca_core::mcp_types::McpServerConfig;

use crate::agent_runtime::TuiAgentRuntime;
use crate::attachment_routing::accept_attached_tui_event;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_projection::{McpCatalogView, McpServerStatusView};
use crate::transcript_state::ChatMessage;
use crate::types::AppState;

/// How long a test waits for what it expects before it fails.
pub(crate) const WAIT: Duration = Duration::from_secs(20);

/// A stdio MCP server named `name`, run by `/bin/sh` from a script in
/// `dir`. It adds its process id to `<dir>/pids` as it starts, waits
/// `delay_secs`, and then offers one tool, `search`, and one prompt,
/// `review_pr <pr>`, which it expands to "Review pull request <pr>."
/// While `<dir>/refuse` exists, it exits at once instead, and while
/// `<dir>/silent` exists, it never answers.
#[cfg(unix)]
pub(crate) fn mcp_server(name: &str, dir: &Path, delay_secs: u64) -> McpServerConfig {
    let script = dir.join("server.sh");
    std::fs::write(
        &script,
        r#"state_dir="$1"
printf '%s\n' "$$" >> "$state_dir/pids"
[ -f "$state_dir/refuse" ] && exit 0
if [ -f "$state_dir/silent" ]; then
  while IFS= read -r line; do :; done
  exit 0
fi
sleep "$2"
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"prompts":{}},"serverInfo":{"name":"docs","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"prompts/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"prompts":[{"name":"review_pr","arguments":[{"name":"pr","required":true}]}]}}\n' "$id"
      ;;
    *'"method":"prompts/get"'*)
      pr=${line#*'"pr":"'}
      pr=${pr%%'"'*}
      printf '{"jsonrpc":"2.0","id":%s,"result":{"messages":[{"role":"user","content":{"type":"text","text":"Review pull request %s."}}]}}\n' "$id" "$pr"
      ;;
  esac
done
"#,
    )
    .expect("write the MCP fixture");
    McpServerConfig {
        name: name.to_string(),
        command: Some("/bin/sh".to_string()),
        args: vec![
            script.to_string_lossy().into_owned(),
            dir.to_string_lossy().into_owned(),
            delay_secs.to_string(),
        ],
        startup_timeout_ms: Some(15_000),
        tool_timeout_ms: Some(15_000),
        ..McpServerConfig::default()
    }
}

/// A stdio MCP server named `name` that reads `initialize` and exits
/// without answering it.
#[cfg(unix)]
pub(crate) fn exiting_mcp_server(name: &str, dir: &Path) -> McpServerConfig {
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(&script, "read -r line\nexit 0\n").expect("write the MCP fixture");
    McpServerConfig {
        name: name.to_string(),
        command: Some("/bin/sh".to_string()),
        args: vec![script.to_string_lossy().into_owned()],
        startup_timeout_ms: Some(15_000),
        ..McpServerConfig::default()
    }
}

/// The process ids the server of [`mcp_server`] in `dir` started
/// with, in order.
#[cfg(unix)]
pub(crate) fn launches(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("pids"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Waits for the server in `dir` to have started `count` times, and
/// returns their process ids.
#[cfg(unix)]
pub(crate) fn wait_for_launches(dir: &Path, count: usize) -> Vec<String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let pids = launches(dir);
        if pids.len() >= count {
            return pids;
        }
        assert!(Instant::now() < deadline, "{pids:?}: not {count} launches");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
pub(crate) fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Waits up to `within` for each of `pids` to be gone.
#[cfg(unix)]
pub(crate) fn wait_until_gone(pids: &[String], within: Duration) {
    let deadline = Instant::now() + within;
    while pids.iter().any(|pid| alive(pid)) {
        assert!(
            Instant::now() < deadline,
            "MCP server processes still running: {:?}",
            pids.iter().filter(|pid| alive(pid)).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The status the catalog in view gives `server`.
pub(crate) fn status_of(state: &AppState, server: &str) -> Option<McpServerStatusView> {
    state
        .mcp_catalog
        .servers
        .iter()
        .find(|listed| listed.name == server)
        .map(|listed| listed.status.clone())
}

pub(crate) fn connected(state: &AppState, server: &str) -> bool {
    status_of(state, server) == Some(McpServerStatusView::Connected)
}

/// The notices the conversation shows.
pub(crate) fn notices(state: &AppState) -> Vec<&str> {
    state
        .transcript
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::System { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The config of a TUI started in `home` with `servers`.
pub(crate) fn config(home: &Path, servers: Vec<McpServerConfig>) -> RunConfig {
    let mut config = crate::test_support::test_run_config();
    config.cwd = Some(home.to_path_buf());
    config.history_mode = orca_core::config::HistoryMode::Record;
    config.api_key = Some("sk-test".to_string());
    config.mcp_servers = servers;
    config
}

/// `orca` with `config`, before its first message: the controller
/// and its runtime as `orca` runs them, and the renderer's state,
/// shown each event as `orca` shows it.
pub(crate) struct Tui {
    pub(crate) state: AppState,
    config: RunConfig,
    events: crossbeam_channel::Receiver<TuiEvent>,
    runtime: Option<TuiAgentRuntime>,
}

impl Tui {
    pub(crate) fn start(config: RunConfig) -> Self {
        let (event_tx, events) = crossbeam_channel::unbounded();
        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Mutex::new(config.clone()));
        let runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx.clone(),
            8,
            TuiSurfaceTaskControl::new(),
            move |control, commands, host| {
                crate::hosted_controller::hosted_tui_controller_loop(
                    shared,
                    Arc::new(Mutex::new(None)),
                    event_tx,
                    commands,
                    control,
                    crate::bridge::PendingWorkflowNotifications::new(),
                    host,
                );
            },
        )
        .expect("hosted TUI runtime");
        Self {
            state: AppState::new(
                action_tx,
                "test".to_string(),
                "mock".to_string(),
                "/tmp".to_string(),
            ),
            config,
            events,
            runtime: Some(runtime),
        }
    }

    pub(crate) fn control(&self) -> TuiSurfaceTaskControl {
        self.runtime
            .as_ref()
            .expect("the TUI is running")
            .controller()
            .clone()
    }

    /// Shows the renderer what comes until `done`.
    pub(crate) fn until(&mut self, what: &str, done: impl Fn(&AppState) -> bool) {
        self.show_until(what, |state, _| done(state));
    }

    /// Shows the renderer what comes until an event `matches`.
    pub(crate) fn until_event(&mut self, what: &str, matches: impl Fn(&TuiEvent) -> bool) {
        self.show_until(what, |_, event| event.is_some_and(&matches));
    }

    fn show_until(&mut self, what: &str, done: impl Fn(&AppState, Option<&TuiEvent>) -> bool) {
        let deadline = Instant::now() + WAIT;
        if done(&self.state, None) {
            return;
        }
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {what}: {:?}",
                self.state.transcript.messages
            );
            let Ok(event) = self
                .events
                .recv_timeout(left.min(Duration::from_millis(50)))
            else {
                continue;
            };
            let Ok(Some(event)) = accept_attached_tui_event(&mut self.state, event) else {
                continue;
            };
            let seen = event.clone();
            self.state.update(event);
            if done(&self.state, Some(&seen)) {
                return;
            }
        }
    }

    /// Shows the renderer what comes for `long`, telling `seen` of each
    /// event.
    pub(crate) fn show_for(&mut self, long: Duration, mut seen: impl FnMut(&TuiEvent)) {
        let deadline = Instant::now() + long;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            let Ok(event) = self.events.recv_timeout(left) else {
                continue;
            };
            let Ok(Some(event)) = accept_attached_tui_event(&mut self.state, event) else {
                continue;
            };
            seen(&event);
            self.state.update(event);
        }
    }

    /// Runs the slash command `command`, as the composer does.
    pub(crate) fn command(&mut self, command: &str) {
        let action_tx = self.state.event_tx.clone();
        let shared = Arc::new(Mutex::new(self.config.clone()));
        crate::slash_command_actions::handle_slash_command(
            command,
            &mut self.config,
            &shared,
            &mut self.state,
            &action_tx,
        );
    }

    /// Sends `text` as the user's message, as the composer does.
    pub(crate) fn send(&mut self, text: &str) {
        let action_tx = self.state.event_tx.clone();
        crate::idle_submit_actions::submit_user_message(
            &mut self.state,
            &action_tx,
            text.to_string(),
            text.to_string(),
            orca_runtime::mentions::MentionBindings::new(text),
            Vec::new(),
        );
    }

    /// Presses `key` in the `/mcp` panel.
    pub(crate) fn press(&mut self, key: char) {
        super::press_in_mcp_panel(&mut self.state, KeyCode::Char(key));
    }

    /// The thread the conversation has, with its catalog once
    /// `ready` accepts it.
    pub(crate) fn thread_catalog(&self, ready: impl Fn(&McpCatalogView) -> bool) -> McpCatalogView {
        let thread = self
            .control()
            .runtime_thread()
            .expect("the conversation has a thread");
        let deadline = Instant::now() + WAIT;
        loop {
            let snapshot = crate::surface_client::read_snapshot(&thread.typed_surface())
                .expect("thread snapshot");
            let catalog = McpCatalogView::from_surface(&snapshot.mcp_catalog);
            if ready(&catalog) {
                return catalog;
            }
            assert!(Instant::now() < deadline, "{catalog:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Quits, as `orca` does.
    pub(crate) fn quit(&mut self) {
        drop(self.runtime.take());
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.quit();
    }
}
