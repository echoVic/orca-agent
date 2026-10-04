//! The MCP servers of a conversation that has not started yet. A new
//! conversation's runtime thread starts with its first message, but its MCP
//! servers start with the TUI: they connect in the background, `/mcp` shows
//! them as they do and acts on them, and their prompts run, all before the
//! first message. The first thread to start then takes them over, so none
//! connects twice; quitting before that stops them.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crossbeam_channel as mpsc;
use orca_core::mcp_types::McpServerConfig;
use orca_mcp::{McpChangeSubscription, McpRegistry};
use orca_runtime::runtime_host::{RuntimeThreadHandle, RuntimeThreadStartRequest};

use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::TuiEvent;
use crate::surface_projection::McpCatalogView;

/// The MCP servers that started with the TUI, until a thread takes them.
pub(crate) struct PrestartedMcp {
    registry: McpRegistry,
    /// Tells the renderer of each change to the servers. Dropping it, as
    /// handing the servers to a thread does, ends that.
    _subscription: McpChangeSubscription,
}

impl PrestartedMcp {
    pub(crate) fn registry(&self) -> &McpRegistry {
        &self.registry
    }

    /// The servers, for a thread to take: the renderer hears no more of
    /// them from here.
    pub(crate) fn into_registry(self) -> McpRegistry {
        self.registry
    }
}

impl fmt::Debug for PrestartedMcp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrestartedMcp")
            .finish_non_exhaustive()
    }
}

/// Starts connecting the MCP servers of `configs` in the background, with
/// the logins saved in `credentials_path`, for the conversation that has
/// not started yet. `event_tx` hears of each change, as
/// `McpCatalogPrestart`, starting with how they stand now, and, once none
/// is still connecting, what their startup left to report, as
/// `StartupWarning`. A disabled server is listed, as the thread's catalog
/// lists it, and never connected. `None` when the config has no MCP server:
/// there is nothing to list.
pub(crate) fn start_prestart_mcp(
    configs: &[McpServerConfig],
    credentials_path: Option<PathBuf>,
    event_tx: mpsc::Sender<TuiEvent>,
) -> Option<PrestartedMcp> {
    if configs.is_empty() {
        return None;
    }
    let registry = orca_mcp::initialize_registry(configs, credentials_path);
    let report = Arc::new(PrestartReport {
        event_tx,
        startup_reported: Mutex::new(false),
    });
    // It holds no registry: the subscription must not keep the servers.
    let subscription = registry.subscribe(Arc::new({
        let report = Arc::clone(&report);
        move |registry: &McpRegistry| report.send(registry)
    }));
    // How they stand now, with what changed before the subscription began.
    report.send(&registry);
    Some(PrestartedMcp {
        registry,
        _subscription: subscription,
    })
}

/// Tells the renderer how the MCP servers that started with the TUI stand.
struct PrestartReport {
    event_tx: mpsc::Sender<TuiEvent>,
    /// Whether what their startup left has been reported. It is held while
    /// the servers are read and sent, so that what is sent last was read
    /// last: changes are told on whichever thread made them.
    startup_reported: Mutex<bool>,
}

impl PrestartReport {
    fn send(&self, registry: &McpRegistry) {
        let mut startup_reported = self
            .startup_reported
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .event_tx
            .send(TuiEvent::McpCatalogPrestart(McpCatalogView::from_registry(
                registry,
            )));
        // Once none is starting, what their startup left is said, in the
        // words of the thread that takes them, which says it again when it
        // is ready: the conversation shows each warning once.
        if !*startup_reported && !registry.is_starting() {
            *startup_reported = true;
            for warning in orca_runtime::mcp_startup_warnings(registry) {
                let _ = self.event_tx.send(TuiEvent::StartupWarning(warning));
            }
        }
    }
}

/// The MCP servers `/mcp` and the MCP prompt commands act on.
#[derive(Clone)]
pub(crate) enum McpServers {
    /// Those of the runtime thread in view.
    Thread(Box<RuntimeThreadHandle>),
    /// Before a thread has started, those that started with the TUI.
    Prestarted(McpRegistry),
}

/// `request`, with the MCP servers that started with the TUI while no
/// thread has taken them yet. They are only lent: once the thread has
/// started, and is the conversation's, the caller hands them over with
/// [`TuiSurfaceTaskControl::take_prestart_mcp`]. Until then, a start that
/// fails leaves them running for the next.
pub(crate) fn offer_prestarted_mcp(
    request: RuntimeThreadStartRequest,
    control: &TuiSurfaceTaskControl,
) -> RuntimeThreadStartRequest {
    match control.prestart_mcp_registry() {
        Some(registry) => request.with_mcp_registry(registry),
        None => request,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::surface_projection::{
        McpCatalogView, McpServerStatusView, McpServerView, SurfaceProjectionState,
    };
    use crate::types::AppState;

    fn app_state() -> AppState {
        AppState::new(
            crossbeam_channel::unbounded().0,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    fn catalog(name: &str, status: McpServerStatusView) -> McpCatalogView {
        McpCatalogView {
            servers: vec![McpServerView {
                name: name.to_string(),
                status,
            }],
            ..McpCatalogView::default()
        }
    }

    /// A projection of a thread whose catalog is `mcp_catalog`.
    fn thread_projection(mcp_catalog: McpCatalogView) -> SurfaceProjectionState {
        SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("session-1".to_string()),
            title: "New conversation".to_string(),
            usage_revision: 0,
            usage: Default::default(),
            context_revision: 0,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
            mcp_catalog,
        }
    }

    #[test]
    fn no_configured_server_starts_nothing() {
        let (event_tx, events) = crossbeam_channel::unbounded();

        assert!(start_prestart_mcp(&[], None, event_tx).is_none());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn before_the_first_message_disabled_servers_are_listed() {
        let (event_tx, events) = crossbeam_channel::unbounded();
        // Never run: it is disabled.
        let archive = McpServerConfig {
            name: "Archive".to_string(),
            command: Some("archive-mcp-server".to_string()),
            disabled: true,
            ..McpServerConfig::default()
        };

        // The panel lists it from launch, as the thread's catalog will,
        // though nothing connects.
        let prestarted = start_prestart_mcp(std::slice::from_ref(&archive), None, event_tx)
            .expect("a configured server is listed before the first message");
        assert!(!prestarted.registry().is_starting());
        let mut state = app_state();
        for event in events.try_iter() {
            state.update(event);
        }
        assert_eq!(
            state.mcp_catalog,
            catalog("archive", McpServerStatusView::Disabled)
        );

        let mut config = crate::test_support::test_run_config();
        config.mcp_servers = vec![archive];
        let shared = Arc::new(Mutex::new(config.clone()));
        let action_tx = state.event_tx.clone();
        crate::slash_command_actions::handle_slash_command(
            "/mcp",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = tui_textarea::TextArea::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            screen
                .iter()
                .any(|row| row.contains("› Archive") && row.contains("disabled")),
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn the_thread_s_catalog_wins_over_a_late_prestart_one() {
        let mut state = app_state();

        // Before a thread, `/mcp` shows the servers that started with the
        // TUI, as they connect.
        state.update(TuiEvent::McpCatalogPrestart(catalog(
            "docs",
            McpServerStatusView::Starting,
        )));
        assert_eq!(
            state.mcp_catalog,
            catalog("docs", McpServerStatusView::Starting)
        );
        state.update(TuiEvent::McpCatalogPrestart(catalog(
            "docs",
            McpServerStatusView::Connected,
        )));
        assert_eq!(
            state.mcp_catalog,
            catalog("docs", McpServerStatusView::Connected)
        );

        // Once a thread's catalog is in view, a prestart catalog sent before
        // the thread took the servers changes nothing.
        let thread_catalog = catalog("docs", McpServerStatusView::NeedsLogin);
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            thread_projection(thread_catalog.clone()),
        )));
        state.update(TuiEvent::McpCatalogPrestart(catalog(
            "docs",
            McpServerStatusView::Connected,
        )));
        assert_eq!(state.mcp_catalog, thread_catalog);
    }

    // The servers are shell scripts.
    #[cfg(unix)]
    pub(crate) mod with_servers {
        use super::*;
        use std::path::Path;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        use crate::agent_runtime::TuiAgentRuntime;
        use crate::attachment_routing::accept_attached_tui_event;
        use crate::transcript_state::ChatMessage;
        use crate::types::AppStatus;

        /// How long a test waits for what it expects before it fails.
        pub(crate) const WAIT: Duration = Duration::from_secs(20);

        /// A stdio MCP server named `name`, run by `/bin/sh` from a script in
        /// `dir`. It adds its process id to `<dir>/pids` as it starts, waits
        /// `delay_secs`, and then offers one tool, `search`, and one prompt,
        /// `review_pr <pr>`, which it expands to "Review pull request <pr>."
        /// While `<dir>/refuse` exists, it exits at once instead.
        pub(crate) fn mcp_server(name: &str, dir: &Path, delay_secs: u64) -> McpServerConfig {
            let script = dir.join("server.sh");
            std::fs::write(
                &script,
                r#"state_dir="$1"
printf '%s\n' "$$" >> "$state_dir/pids"
[ -f "$state_dir/refuse" ] && exit 0
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
        pub(crate) fn launches(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("pids"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// Waits for the server in `dir` to have started `count` times, and
        /// returns their process ids.
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

        pub(crate) fn alive(pid: &str) -> bool {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

        /// Waits up to `within` for each of `pids` to be gone.
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

        use orca_core::config::RunConfig;

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

            fn show_until(
                &mut self,
                what: &str,
                done: impl Fn(&AppState, Option<&TuiEvent>) -> bool,
            ) {
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

            pub(crate) fn press(&mut self, key: char) {
                let action_tx = self.state.event_tx.clone();
                crate::mcp_dialog_actions::handle_mcp_dialog_key(
                    &KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                    &mut self.state,
                    &action_tx,
                );
            }

            /// The thread the conversation has, with its catalog once
            /// `ready` accepts it.
            pub(crate) fn thread_catalog(
                &self,
                ready: impl Fn(&McpCatalogView) -> bool,
            ) -> McpCatalogView {
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

        fn thread_lists_docs_connected(catalog: &McpCatalogView) -> bool {
            catalog.servers
                == [McpServerView {
                    name: "docs".to_string(),
                    status: McpServerStatusView::Connected,
                }]
                && catalog.server_tools("docs").count() == 1
        }

        #[test]
        fn before_the_first_message_the_panel_shows_live_server_status() {
            let fixture = tempfile::tempdir().unwrap();
            let (event_tx, events) = crossbeam_channel::unbounded();
            let prestarted =
                start_prestart_mcp(&[mcp_server("docs", fixture.path(), 0)], None, event_tx)
                    .expect("an enabled server starts with the TUI");
            let mut state = app_state();

            let deadline = Instant::now() + WAIT;
            while !connected(&state, "docs") {
                let left = deadline.saturating_duration_since(Instant::now());
                let event = events.recv_timeout(left).expect("the server's catalog");
                assert!(
                    matches!(event, TuiEvent::McpCatalogPrestart(_)),
                    "{event:?}"
                );
                state.update(event);
            }
            let mut config = crate::test_support::test_run_config();
            config.mcp_servers = vec![mcp_server("docs", fixture.path(), 0)];
            let shared = Arc::new(Mutex::new(config.clone()));
            let action_tx = state.event_tx.clone();
            crate::slash_command_actions::handle_slash_command(
                "/mcp",
                &mut config,
                &shared,
                &mut state,
                &action_tx,
            );

            let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
            let textarea = tui_textarea::TextArea::default();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| crate::ui::render(frame, &mut state, &textarea, &theme))
                .unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .chunks(100)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>();
            assert!(
                screen
                    .iter()
                    .any(|row| row.contains("› docs") && row.contains("connected · 1 tool")),
                "{}",
                screen.join("\n")
            );

            let pids = launches(fixture.path());
            drop(prestarted);
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn the_first_message_hands_the_prestarted_registry_to_the_thread() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));
            tui.until("the server to connect", |state| connected(state, "docs"));

            tui.send("hello");
            tui.until_event("the first turn to end", |event| {
                matches!(event, TuiEvent::SessionCompleted { .. })
            });

            // The thread lists the server, connected, from its own catalog,
            // which `/mcp` now shows: it took the server as it was.
            tui.thread_catalog(thread_lists_docs_connected);
            tui.until("the thread's catalog", |state| {
                state.surface_mcp_catalog_applied && connected(state, "docs")
            });
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "the server started again: {pids:?}");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn new_before_the_first_message_uses_the_prestarted_servers() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));
            tui.until("the server to connect", |state| connected(state, "docs"));

            tui.command("/new");
            tui.until_event("the new conversation", |event| {
                matches!(event, TuiEvent::NewSessionStarted)
            });

            tui.thread_catalog(thread_lists_docs_connected);
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "the server started again: {pids:?}");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn reconnect_before_the_first_message_uses_the_prestarted_registry() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));
            tui.until("the server to connect", |state| connected(state, "docs"));

            tui.command("/mcp");
            tui.press('r');
            assert!(tui.state.mcp_actions_in_flight.contains_key("docs"));
            tui.until("the reconnect to end", |state| {
                state.mcp_actions_in_flight.is_empty()
            });

            assert!(
                notices(&tui.state).contains(&"MCP server docs: connected · 1 tool"),
                "{:?}",
                notices(&tui.state)
            );
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 2, "{pids:?}");
            // The reconnect stopped the first server before it started the
            // second.
            assert!(!alive(&pids[0]), "the first server still runs");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn a_prompt_command_runs_before_the_first_message() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));
            tui.until("the server's prompt", |state| {
                !state.mcp_catalog.prompts.is_empty()
            });

            tui.command("/mcp__docs__review_pr 123");
            tui.until_event("the turn the prompt started to end", |event| {
                matches!(event, TuiEvent::SessionCompleted { .. })
            });

            // Its expansion was the conversation's first message, which
            // started the thread.
            let snapshot = crate::surface_client::read_snapshot(
                &tui.control()
                    .runtime_thread()
                    .expect("the prompt's message started a thread")
                    .typed_surface(),
            )
            .expect("thread snapshot");
            let sent = snapshot
                .items
                .iter()
                .filter_map(|item| match item {
                    orca_runtime::surface::SurfaceItem::UserMessage {
                        input:
                            orca_runtime::surface::SurfaceUserInputState::Resolved {
                                fact:
                                    orca_runtime::surface::SurfaceResolvedInputFact::Replayable {
                                        input,
                                        ..
                                    },
                            },
                        ..
                    } => Some(input.canonical_text.as_str().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(sent, ["Review pull request 123."]);
            assert!(
                tui.state.transcript.messages.iter().any(
                    |message| matches!(message, ChatMessage::User(text) if text == "Review pull request 123.")
                ),
                "{:?}",
                tui.state.transcript.messages
            );
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "the server started again: {pids:?}");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn quitting_before_the_first_message_stops_mcp_servers() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));
            tui.until("the server to connect", |state| connected(state, "docs"));
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "{pids:?}");

            tui.quit();

            // Nothing else holds the servers, so quitting stopped them.
            assert!(!alive(&pids[0]), "the server outlived the TUI");
        }

        #[test]
        fn quitting_while_servers_are_still_starting_stops_them() {
            let home = crate::test_support::isolate_orca_home();
            let fixtures = [(); 3].map(|_| tempfile::tempdir().unwrap());
            // Each answers `initialize` two seconds after it starts, well
            // within its startup timeout.
            let servers = fixtures
                .iter()
                .enumerate()
                .map(|(index, fixture)| mcp_server(&format!("slow{index}"), fixture.path(), 2))
                .collect();
            let mut tui = Tui::start(config(home.path(), servers));
            let pids = fixtures
                .iter()
                .flat_map(|fixture| wait_for_launches(fixture.path(), 1))
                .collect::<Vec<_>>();
            assert!(
                tui.state
                    .mcp_catalog
                    .servers
                    .iter()
                    .all(|server| { server.status != McpServerStatusView::Connected })
            );

            tui.quit();

            // A connection under way ends with its server's answer, once
            // nothing wants it: none outlives its two seconds by much.
            wait_until_gone(&pids, Duration::from_millis(2_500));
        }

        #[test]
        fn startup_warnings_come_once_whoever_reports_them() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let warning = "MCP server 'broken': MCP server closed stdout";
            let mut tui = Tui::start(config(
                home.path(),
                vec![
                    exiting_mcp_server("broken", fixture.path()),
                    mcp_server("docs", fixture.path(), 0),
                ],
            ));

            let reported = std::cell::Cell::new(0);
            let count = |event: &TuiEvent| {
                if matches!(event, TuiEvent::StartupWarning(said) if said == warning) {
                    reported.set(reported.get() + 1);
                }
            };

            // Startup ends before the first message: the warning is the
            // runtime's, in its words.
            tui.until_event("the startup warning", |event| {
                count(event);
                reported.get() == 1
            });
            assert!(notices(&tui.state).contains(&warning));
            // The thread reports its startup too, once it is ready, with
            // the same words: the conversation says it once.
            tui.send("hello");
            tui.until_event("the first turn to end", |event| {
                count(event);
                matches!(event, TuiEvent::SessionCompleted { .. })
            });
            assert_eq!(reported.get(), 2, "the thread did not report its startup");
            assert_eq!(
                notices(&tui.state)
                    .iter()
                    .filter(|notice| **notice == warning)
                    .count(),
                1,
                "{:?}",
                notices(&tui.state)
            );
            assert_eq!(tui.state.status, AppStatus::Idle);
            let pids = launches(fixture.path());
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }
    }
}
