//! The MCP servers of a conversation that has not started yet. A new
//! conversation's runtime thread starts with its first message, but its MCP
//! servers start with the TUI: they connect in the background, `/mcp` shows
//! them as they do and acts on them, and their prompts run, all before the
//! first message. The first thread to start then takes them over, so none
//! connects twice, and stops them when it ends; quitting before that stops
//! them, those still connecting too.

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

    /// Stops the servers, those still connecting too, however many workers
    /// still hold them: the renderer hears no more of them.
    pub(crate) fn close(self) {
        self.registry.close();
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
/// started, and is announced as the conversation's, they are handed over
/// with [`TuiSurfaceTaskControl::hand_over_prestart_mcp`]. Until then, a
/// start that fails, or a thread shut down before then, leaves them running
/// for the next.
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
mod tests {
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
                prompts_error: None,
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
        let screen = crate::test_support::frame_string(&mut state, 100, 30);
        assert!(
            screen
                .lines()
                .any(|row| row.contains("› Archive") && row.contains("disabled")),
            "{screen}"
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
    mod with_servers {
        use super::*;
        use std::time::{Duration, Instant};

        use crate::test_support::hosted_tui::{
            Tui, WAIT, alive, config, connected, exiting_mcp_server, launches, mcp_server, notices,
            wait_for_launches, wait_until_gone,
        };
        use crate::transcript_state::ChatMessage;
        use crate::types::AppStatus;

        fn thread_lists_docs_connected(catalog: &McpCatalogView) -> bool {
            catalog.servers
                == [McpServerView {
                    name: "docs".to_string(),
                    status: McpServerStatusView::Connected,
                    prompts_error: None,
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

            let screen = crate::test_support::frame_string(&mut state, 100, 30);
            assert!(
                screen
                    .lines()
                    .any(|row| row.contains("› docs") && row.contains("connected · 1 tool")),
                "{screen}"
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
            // Each answers `initialize` only after its startup timeout, so
            // its connection would wait that long for it.
            let servers = fixtures
                .iter()
                .enumerate()
                .map(|(index, fixture)| mcp_server(&format!("slow{index}"), fixture.path(), 30))
                .collect();
            let mut tui = Tui::start(config(home.path(), servers));
            let pids = fixtures
                .iter()
                .flat_map(|fixture| wait_for_launches(fixture.path(), 1))
                .collect::<Vec<_>>();
            tui.until("three servers starting", |state| {
                state.mcp_catalog.servers.len() == 3
                    && state
                        .mcp_catalog
                        .servers
                        .iter()
                        .all(|server| server.status == McpServerStatusView::Starting)
            });
            // None has connected yet as the TUI quits.
            assert!(
                tui.control()
                    .prestart_mcp_registry()
                    .is_some_and(|registry| registry
                        .server_statuses()
                        .iter()
                        .all(|status| status.state == orca_mcp::McpServerState::Starting)),
                "a server connected before the TUI quit"
            );

            tui.quit();

            // Quitting stopped them, though their connections were still
            // under way, on threads `orca` does not wait for as it exits.
            wait_until_gone(&pids, Duration::from_secs(1));
        }

        /// Has the controller answer an action sent after every one before
        /// it: once it has, it has acted on those, and on its launch.
        fn settle(tui: &mut Tui) {
            tui.state
                .event_tx
                .send(crate::protocol::UserAction::GoalShow)
                .expect("the controller runs");
            tui.until_event("the controller to answer", |event| {
                matches!(event, TuiEvent::GoalStatus(None))
            });
        }

        #[test]
        fn mcp_servers_wait_for_first_run_setup_and_then_start_once() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start_in_setup(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));

            // The workspace is not accepted yet: nothing it configures runs.
            settle(&mut tui);
            assert!(tui.control().prestart_mcp_registry().is_none());
            assert_eq!(launches(fixture.path()), Vec::<String>::new());

            // Accepting it, with a key set, ends setup.
            assert!(matches!(
                tui.press_in_setup(crossterm::event::KeyCode::Enter),
                crate::setup_actions::SetupFlow::Continue
            ));
            assert_eq!(tui.state.status, AppStatus::Idle);
            tui.until("the server to connect", |state| connected(state, "docs"));
            let started = tui
                .control()
                .prestart_mcp_registry()
                .expect("the servers that started");
            // Setup ends once: word of it again starts nothing more.
            tui.state
                .event_tx
                .send(crate::protocol::UserAction::SetupFinished)
                .expect("the controller runs");
            settle(&mut tui);
            assert!(
                tui.control()
                    .prestart_mcp_registry()
                    .is_some_and(|registry| registry.is_same(&started)),
                "the servers started again"
            );
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "{pids:?}");
            assert!(alive(&pids[0]), "the server was stopped");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        #[test]
        fn choosing_exit_on_first_run_setup_starts_no_mcp_server() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut tui = Tui::start_in_setup(config(
                home.path(),
                vec![mcp_server("docs", fixture.path(), 0)],
            ));

            assert!(matches!(
                tui.press_in_setup(crossterm::event::KeyCode::Char('e')),
                crate::setup_actions::SetupFlow::Exit(0)
            ));
            settle(&mut tui);
            assert!(tui.control().prestart_mcp_registry().is_none());
            tui.quit();

            assert_eq!(launches(fixture.path()), Vec::<String>::new());
        }

        #[test]
        fn the_servers_change_hands_once_the_thread_is_the_conversation_s() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let config = config(home.path(), vec![mcp_server("docs", fixture.path(), 0)]);
            let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
            let control = TuiSurfaceTaskControl::new();
            let (event_tx, _events) = crossbeam_channel::unbounded();
            let prestarted = start_prestart_mcp(&config.mcp_servers, None, event_tx.clone())
                .expect("an enabled server starts with the TUI");
            let registry = prestarted.registry().clone();
            control.set_prestart_mcp(prestarted);
            let is_prestarted = |servers: Option<McpServers>| matches!(servers, Some(McpServers::Prestarted(servers)) if servers.is_same(&registry));

            let mut thread = None;
            crate::hosted_session_lifecycle::ensure_hosted_thread(
                &mut thread,
                &host.handle(),
                &config,
                &Arc::new(Mutex::new(None)),
                "hello",
                &event_tx,
                &control,
            )
            .expect("start the thread");
            let started = thread.as_ref().expect("the thread");

            // Started, with the servers lent to it, but not yet the
            // conversation's: `/mcp` still acts on them through the TUI.
            assert!(started.mcp_registry().is_same(&registry));
            assert!(is_prestarted(control.mcp_servers()));

            crate::hosted_session::announce_runtime_ready(started, &event_tx, &control);

            // Now through the thread, which has them for good.
            assert!(control.prestart_mcp_registry().is_none());
            assert!(matches!(
                control.mcp_servers(),
                Some(McpServers::Thread(bound)) if bound.mcp_registry().is_same(&registry)
            ));
            let pids = wait_for_launches(fixture.path(), 1);
            control.shutdown();
            control.release_runtime_thread();
            drop(thread);
            host.shutdown().expect("shut the runtime host down");
            // The thread took them over, so its end stopped them.
            wait_until_gone(&pids, Duration::from_secs(1));
        }

        #[test]
        fn quitting_while_the_thread_reconnects_a_server_stops_it() {
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
            tui.thread_catalog(thread_lists_docs_connected);
            // Its next start never answers.
            std::fs::write(fixture.path().join("silent"), "").expect("silence the server");

            tui.command("/mcp");
            tui.press('r');
            let pids = wait_for_launches(fixture.path(), 2);

            tui.quit();

            // The thread took the servers over, and stopped them as it
            // ended: the one its reconnect was still starting too, which the
            // reconnect's worker still held.
            wait_until_gone(&pids, Duration::from_secs(1));
        }

        #[test]
        fn a_failed_start_keeps_the_prestarted_servers_for_the_next() {
            let home = crate::test_support::isolate_orca_home();
            let fixture = tempfile::tempdir().unwrap();
            let mut config = config(home.path(), vec![mcp_server("docs", fixture.path(), 0)]);
            // The first message's thread cannot start: the conversation it
            // would resume is not there.
            config.history_mode =
                orca_core::config::HistoryMode::Resume("no-such-conversation".to_string());
            let mut tui = Tui::start(config);
            tui.until("the server to connect", |state| connected(state, "docs"));

            tui.send("hello");
            tui.until_event("the thread to fail to start", |event| {
                matches!(event, TuiEvent::SubmissionRejected { message, .. }
                    if message.starts_with("failed to initialize conversation history"))
            });

            // The servers are still the TUI's, still connected.
            assert!(tui.control().prestart_mcp_registry().is_some());
            assert!(tui.control().runtime_thread().is_none());
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 1, "{pids:?}");
            assert!(alive(&pids[0]), "the failed start stopped the server");
            // The next start takes them as they are.
            tui.command("/new");
            tui.until_event("the new conversation", |event| {
                matches!(event, TuiEvent::NewSessionStarted)
            });
            tui.thread_catalog(thread_lists_docs_connected);
            assert!(tui.control().prestart_mcp_registry().is_none());
            assert_eq!(launches(fixture.path()), pids, "the server started again");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
        }

        /// The session id of a conversation an earlier `orca`, with no MCP
        /// server, saved in `home`.
        fn a_saved_conversation(home: &std::path::Path) -> String {
            let mut earlier = Tui::start(config(home, Vec::new()));
            earlier.send("an earlier conversation");
            earlier.until_event("its turn to end", |event| {
                matches!(event, TuiEvent::SessionCompleted { .. })
            });
            let saved = earlier
                .control()
                .runtime_thread()
                .and_then(|thread| thread.session_id().map(str::to_string))
                .expect("the earlier conversation is saved");
            earlier.quit();
            saved
        }

        #[test]
        fn resuming_or_forking_a_saved_conversation_first_uses_the_prestarted_servers() {
            use crate::protocol::UserAction;

            let home = crate::test_support::isolate_orca_home();
            let saved = a_saved_conversation(home.path());
            for (what, action) in [
                (
                    "resume",
                    UserAction::ResumeSavedSession {
                        session_id: saved.clone(),
                    },
                ),
                (
                    "fork",
                    UserAction::ForkSavedSession {
                        session_id: saved.clone(),
                    },
                ),
            ] {
                let fixture = tempfile::tempdir().unwrap();
                let mut tui = Tui::start(config(
                    home.path(),
                    vec![mcp_server("docs", fixture.path(), 0)],
                ));
                tui.until("the server to connect", |state| connected(state, "docs"));

                // From the session picker, before any message. Its history
                // comes after the thread is announced, with an event that
                // holds the thread: one left unread would keep its servers
                // running after the quit.
                tui.state
                    .event_tx
                    .send(action)
                    .expect("the controller runs");
                tui.until_event("the saved conversation's history", |event| {
                    matches!(event, TuiEvent::HistoryLoaded { .. })
                });

                tui.thread_catalog(thread_lists_docs_connected);
                assert!(tui.control().prestart_mcp_registry().is_none(), "{what}");
                let pids = launches(fixture.path());
                assert_eq!(pids.len(), 1, "{what}: the server started again: {pids:?}");
                tui.quit();
                wait_until_gone(&pids, Duration::from_secs(5));
            }
        }

        #[test]
        fn the_prestart_catalog_stops_once_a_thread_has_the_servers() {
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
            tui.thread_catalog(thread_lists_docs_connected);

            // The thread's servers change: only the thread tells of it now.
            let prestart_catalogs = std::cell::Cell::new(0);
            let count = |event: &TuiEvent| {
                if matches!(event, TuiEvent::McpCatalogPrestart(_)) {
                    prestart_catalogs.set(prestart_catalogs.get() + 1);
                }
            };
            tui.command("/mcp");
            tui.press('r');
            tui.until_event("the reconnect to end", |event| {
                count(event);
                matches!(event, TuiEvent::McpActionFinished { .. })
            });
            tui.show_for(Duration::from_millis(200), count);

            assert_eq!(prestart_catalogs.get(), 0);
            let pids = launches(fixture.path());
            assert_eq!(pids.len(), 2, "{pids:?}");
            tui.quit();
            wait_until_gone(&pids, Duration::from_secs(5));
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
