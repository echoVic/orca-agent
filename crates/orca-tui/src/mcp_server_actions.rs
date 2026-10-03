//! What `/mcp`'s reconnect, log in and log out do. Each runs on a worker of
//! its own, off the UI thread and the action dispatcher: a reconnect takes
//! up to the server's startup timeout, and a login waits for the browser
//! for up to five minutes. Each says how it went in notices, and ends with
//! `McpActionFinished`, which lets `/mcp` act on the server again. Before
//! the conversation's runtime starts, there is nothing to reconnect: its
//! servers connect when it does, with whatever login is saved by then.

use std::io;
use std::path::Path;

use crossbeam_channel::Sender;
use orca_core::config::mcp_credentials::{delete_mcp_credential, mcp_credentials_path};
use orca_core::mcp_types::McpServerConfig;
use orca_mcp::oauth::{McpLoginOptions, OpenBrowser};
use orca_runtime::runtime_host::RuntimeThreadHandle;
use orca_runtime::surface::SurfaceMcpServerStatus;

use crate::protocol::{TuiEvent, UserAction};
use crate::surface_projection::McpServerStatusView;

const NO_CONFIG_DIR: &str = "could not resolve the Orca configuration directory";

/// What `r` answers before the conversation's runtime starts.
pub(crate) const MCP_SERVERS_NOT_STARTED: &str =
    "MCP servers connect when the conversation starts; send a message first.";

/// A `/mcp` action whose worker could not start. Nothing else frees its
/// server, so the caller must deliver `McpActionFinished` for it.
#[derive(Debug)]
pub(crate) struct McpActionNotStarted {
    /// The catalog's name for the server.
    pub(crate) server: String,
    /// Why, for the user.
    pub(crate) notice: String,
}

/// Starts `action`, a `McpReconnect`, `McpLogin` or `McpLogout`, on the
/// MCP servers of `runtime`, the thread in view, on a worker of its own.
/// Closing `/mcp` does not stop it.
pub(crate) fn spawn_mcp_server_action(
    action: UserAction,
    runtime: Option<RuntimeThreadHandle>,
    event_tx: Sender<TuiEvent>,
) -> Result<(), McpActionNotStarted> {
    let Some(name) = action_server_name(&action).map(str::to_string) else {
        return Ok(());
    };
    std::thread::Builder::new()
        .name("orca-tui-mcp-action".to_string())
        .spawn(move || {
            run_mcp_server_worker(action, runtime.as_ref(), &event_tx, |server| {
                log_in_with_browser(server, &event_tx)
            });
        })
        .map(|_| ())
        .map_err(|error| McpActionNotStarted {
            server: orca_mcp::canonical_server_name(&name),
            notice: format!("failed to start the MCP action for {name}: {error}"),
        })
}

/// The name `action` gives its server: the config name, or the catalog's.
fn action_server_name(action: &UserAction) -> Option<&str> {
    match action {
        UserAction::McpReconnect { server } | UserAction::McpLogout { server } => Some(server),
        UserAction::McpLogin { server } => Some(&server.name),
        _ => None,
    }
}

/// A worker's whole run: `action`, then `McpActionFinished` for its server,
/// however the action ends, a panic included.
fn run_mcp_server_worker(
    action: UserAction,
    runtime: Option<&RuntimeThreadHandle>,
    event_tx: &Sender<TuiEvent>,
    log_in: impl FnOnce(&McpServerConfig) -> Result<(), String>,
) {
    let Some(name) = action_server_name(&action) else {
        return;
    };
    let _finished = FinishOnDrop {
        event_tx: event_tx.clone(),
        server: orca_mcp::canonical_server_name(name),
    };
    run_mcp_server_action(action, runtime, event_tx, log_in);
}

/// Sends `McpActionFinished` for `server` when dropped, which frees the
/// server in `/mcp` on every way out of a worker, unwinding included.
struct FinishOnDrop {
    event_tx: Sender<TuiEvent>,
    server: String,
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        // A worker's own thread may wait for room in the event channel.
        let _ = self.event_tx.send(TuiEvent::McpActionFinished {
            server: std::mem::take(&mut self.server),
        });
    }
}

/// Runs `action` on the MCP servers of `runtime`, logging in with
/// `log_in`. Without a runtime, which starts with the conversation, a
/// login or logout reconnects nothing.
fn run_mcp_server_action(
    action: UserAction,
    runtime: Option<&RuntimeThreadHandle>,
    event_tx: &Sender<TuiEvent>,
    log_in: impl FnOnce(&McpServerConfig) -> Result<(), String>,
) {
    let notice = |text: String| {
        let _ = event_tx.send(TuiEvent::Notice(text));
    };
    let reconnect = |server: String| {
        runtime.map(|runtime| {
            move || {
                let result =
                    crate::surface_client::reconnect_mcp_server(&runtime.typed_surface(), &server);
                reconnect_notice(&server, result)
            }
        })
    };
    match action {
        UserAction::McpReconnect { server } => match reconnect(server) {
            Some(reconnect) => notice(reconnect()),
            None => notice(MCP_SERVERS_NOT_STARTED.to_string()),
        },
        UserAction::McpLogin { server } => log_in_then_reconnect(
            &server.name,
            || log_in(&server),
            reconnect(server.name.clone()),
            &notice,
        ),
        UserAction::McpLogout { server } => log_out_then_reconnect(
            &server,
            mcp_credentials_path().as_deref(),
            reconnect(server.clone()),
            &notice,
        ),
        _ => {}
    }
}

/// Logs in to `server` in the browser, saving the tokens under its config
/// name as `orca mcp login` does.
fn log_in_with_browser(
    server: &McpServerConfig,
    event_tx: &Sender<TuiEvent>,
) -> Result<(), String> {
    let credentials_path = mcp_credentials_path().ok_or(NO_CONFIG_DIR)?;
    let open_browser = browser_opener(
        orca_mcp::canonical_server_name(&server.name),
        event_tx.clone(),
        orca_platform::process::open_url,
    );
    orca_mcp::oauth::login(server, McpLoginOptions::new(credentials_path, open_browser))
}

/// Opens the authorization url with `open_url`, after handing it to `/mcp`
/// for the server the catalog names `server`, and to the conversation: a
/// browser that does not open leaves the login waiting, for the user to
/// open the url by hand.
fn browser_opener(
    server: String,
    event_tx: Sender<TuiEvent>,
    open_url: fn(&str) -> io::Result<()>,
) -> OpenBrowser {
    Box::new(move |url: &str| {
        let _ = event_tx.send(TuiEvent::McpLoginUrl {
            server,
            url: url.to_string(),
        });
        let _ = event_tx.send(TuiEvent::Notice(format!(
            "If your browser does not open, visit: {url}"
        )));
        open_url(url)
    })
}

/// `l`: logs in to `server` with `login`, and once that succeeds reconnects
/// it with `reconnect`, whose notice it shows. With no `reconnect`, before
/// the conversation's runtime starts, the server takes up the login when it
/// connects. A login that fails is shown, and nothing is reconnected.
fn log_in_then_reconnect(
    server: &str,
    login: impl FnOnce() -> Result<(), String>,
    reconnect: Option<impl FnOnce() -> String>,
    notice: &dyn Fn(String),
) {
    notice(format!("waiting for browser login for {server}…"));
    match (login(), reconnect) {
        (Ok(()), Some(reconnect)) => {
            notice(format!("logged in to MCP server {server}"));
            notice(reconnect());
        }
        (Ok(()), None) => notice(format!(
            "logged in to MCP server {server}; it connects when the conversation starts"
        )),
        (Err(error), _) => notice(error),
    }
}

/// `o`: deletes the login saved in `credentials_path` under `server`, the
/// server's config name, and then reconnects it with `reconnect`, if there
/// is one, whose notice it shows. With no login saved, it says so and
/// reconnects nothing.
fn log_out_then_reconnect(
    server: &str,
    credentials_path: Option<&Path>,
    reconnect: Option<impl FnOnce() -> String>,
    notice: &dyn Fn(String),
) {
    let deleted = credentials_path
        .ok_or_else(|| io::Error::other(NO_CONFIG_DIR))
        .and_then(|path| delete_mcp_credential(path, server));
    match deleted {
        Ok(true) => {
            notice(format!("logged out of MCP server {server}"));
            if let Some(reconnect) = reconnect {
                notice(reconnect());
            }
        }
        Ok(false) => notice(format!("MCP server '{server}' is not logged in")),
        Err(error) => notice(format!("failed to log out of MCP server {server}: {error}")),
    }
}

/// What a reconnect of `server` left it as, in the words of `/mcp`'s list.
/// The panel's list itself changes only with the catalog.
fn reconnect_notice(server: &str, result: Result<SurfaceMcpServerStatus, String>) -> String {
    match result {
        Ok(status) => format!(
            "MCP server {server}: {}",
            McpServerStatusView::from_surface(&status).label()
        ),
        Err(error) => format!("failed to reconnect MCP server {server}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use orca_core::config::mcp_credentials::{
        MCP_CREDENTIALS_FILE, McpCredential, load_mcp_credential, save_mcp_credential,
    };
    use orca_runtime::surface::DisplayText;

    /// Runs `step`, and returns the notices it showed, in order.
    fn notices_of(step: impl FnOnce(&dyn Fn(String))) -> Vec<String> {
        let notices = RefCell::new(Vec::new());
        step(&|text| notices.borrow_mut().push(text));
        notices.into_inner()
    }

    fn credential(server_url: &str) -> McpCredential {
        McpCredential {
            server_url: server_url.to_string(),
            access_token: "at-1".to_string(),
            refresh_token: Some("rt-1".to_string()),
            expires_at: None,
            token_endpoint: "https://auth.example/token".to_string(),
            client_id: "orca".to_string(),
            resource: server_url.to_string(),
            scope: None,
        }
    }

    #[test]
    fn a_login_reconnects_the_server_once_it_succeeds() {
        let reconnects = Cell::new(0);
        let notices = notices_of(|notice| {
            log_in_then_reconnect(
                "linear",
                || Ok(()),
                Some(|| {
                    reconnects.set(reconnects.get() + 1);
                    "MCP server linear: connected".to_string()
                }),
                notice,
            )
        });

        assert_eq!(reconnects.get(), 1);
        assert_eq!(
            notices,
            [
                "waiting for browser login for linear…",
                "logged in to MCP server linear",
                "MCP server linear: connected",
            ]
        );
    }

    #[test]
    fn a_failed_login_reconnects_nothing() {
        let notices = notices_of(|notice| {
            log_in_then_reconnect(
                "linear",
                || Err("timed out waiting for the browser login for MCP server 'linear'".into()),
                Some(|| -> String { panic!("a failed login reconnected the server") }),
                notice,
            )
        });

        assert_eq!(
            notices,
            [
                "waiting for browser login for linear…",
                "timed out waiting for the browser login for MCP server 'linear'",
            ]
        );
    }

    #[test]
    fn logging_out_without_a_saved_login_reconnects_nothing() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join(MCP_CREDENTIALS_FILE);

        let notices = notices_of(|notice| {
            log_out_then_reconnect(
                "linear",
                Some(&credentials),
                Some(|| -> String {
                    panic!("a logout with no login to drop reconnected the server")
                }),
                notice,
            )
        });

        assert_eq!(notices, ["MCP server 'linear' is not logged in"]);
    }

    #[test]
    fn logging_out_drops_the_login_saved_under_the_config_name_then_reconnects() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join(MCP_CREDENTIALS_FILE);
        let url = "https://my-server.example/mcp";
        save_mcp_credential(&credentials, "My-Server", &credential(url)).unwrap();
        save_mcp_credential(&credentials, "my_server", &credential(url)).unwrap();
        let reconnects = Cell::new(0);

        let notices = notices_of(|notice| {
            log_out_then_reconnect(
                "My-Server",
                Some(&credentials),
                Some(|| {
                    reconnects.set(reconnects.get() + 1);
                    "MCP server My-Server: needs login".to_string()
                }),
                notice,
            )
        });

        assert_eq!(reconnects.get(), 1);
        assert_eq!(
            notices,
            [
                "logged out of MCP server My-Server",
                "MCP server My-Server: needs login",
            ]
        );
        assert!(
            load_mcp_credential(&credentials, "My-Server", url)
                .unwrap()
                .is_none()
        );
        // The catalog's name for the server is not the one the login is
        // saved under.
        assert!(
            load_mcp_credential(&credentials, "my_server", url)
                .unwrap()
                .is_some()
        );
    }

    /// `orca mcp add linear --url …`, `orca`, `/mcp`, `l`: no message yet,
    /// so no runtime. Returns the TUI and the login `l` sent.
    fn login_pressed_before_start() -> (crate::types::AppState, UserAction) {
        use std::sync::{Arc, Mutex};

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let mut state = crate::types::AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = crate::test_support::test_run_config();
        config.mcp_servers = vec![orca_core::mcp_types::McpServerConfig {
            name: "linear".to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Http,
            url: Some("https://linear.example/mcp".to_string()),
            ..Default::default()
        }];
        let shared = Arc::new(Mutex::new(config.clone()));
        crate::slash_command_actions::handle_slash_command(
            "/mcp",
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        );
        crate::mcp_dialog_actions::handle_mcp_dialog_key(
            &KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &mut state,
            &action_tx,
        );
        let action = action_rx.try_recv().expect("`l` sent a login");
        assert!(matches!(&action, UserAction::McpLogin { server } if server.name == "linear"));
        assert!(state.mcp_actions_in_flight.contains_key("linear"));
        (state, action)
    }

    #[test]
    fn l_before_the_runtime_starts_logs_in_and_reconnects_nothing() {
        let (_state, action) = login_pressed_before_start();

        // The worker, with the browser login standing in for itself.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        run_mcp_server_action(action, None, &event_tx, |_| Ok(()));

        let notices = event_rx
            .try_iter()
            .map(|event| match event {
                TuiEvent::Notice(notice) => notice,
                other => panic!("the login sent {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices,
            [
                "waiting for browser login for linear…",
                "logged in to MCP server linear; it connects when the conversation starts",
            ]
        );
    }

    #[test]
    fn a_login_that_panics_still_frees_its_server() {
        let (mut state, action) = login_pressed_before_start();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let worker = std::thread::spawn(move || {
            run_mcp_server_worker(action, None, &event_tx, |_| -> Result<(), String> {
                panic!("the browser login panicked")
            })
        });

        assert!(worker.join().is_err(), "the login did not panic");
        for event in event_rx.try_iter() {
            state.update(event);
        }
        assert!(state.mcp_actions_in_flight.is_empty());
    }

    #[test]
    fn the_login_hands_its_url_to_the_panel_before_the_browser_opens() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let url = "https://auth.example/authorize?client_id=orca&state=1";

        let open = browser_opener("linear".to_string(), event_tx, |_| Ok(()));
        open(url).expect("open the url");

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(
                events.as_slice(),
                [
                    TuiEvent::McpLoginUrl { server, url: shown },
                    TuiEvent::Notice(notice),
                ] if server == "linear"
                    && shown == url
                    && *notice == format!("If your browser does not open, visit: {url}")
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_logout_before_the_runtime_starts_reconnects_nothing() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join(MCP_CREDENTIALS_FILE);
        let url = "https://linear.example/mcp";
        save_mcp_credential(&credentials, "linear", &credential(url)).unwrap();

        let notices = notices_of(|notice| {
            log_out_then_reconnect("linear", Some(&credentials), None::<fn() -> String>, notice)
        });

        assert_eq!(notices, ["logged out of MCP server linear"]);
        assert!(
            load_mcp_credential(&credentials, "linear", url)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconnect_results_read_as_the_server_status_they_leave() {
        let cases = [
            (
                Ok(SurfaceMcpServerStatus::Ready),
                "MCP server docs: connected",
            ),
            (
                Ok(SurfaceMcpServerStatus::Failed {
                    message: DisplayText::new("no MCP server named 'docs'"),
                }),
                "MCP server docs: failed: no MCP server named 'docs'",
            ),
            (
                Ok(SurfaceMcpServerStatus::NeedsLogin),
                "MCP server docs: needs login",
            ),
            (
                Ok(SurfaceMcpServerStatus::Disabled),
                "MCP server docs: disabled",
            ),
            (
                Err("the conversation is unavailable".to_string()),
                "failed to reconnect MCP server docs: the conversation is unavailable",
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(reconnect_notice("docs", result), expected);
        }
    }

    #[test]
    fn an_action_ends_with_its_server_freed_even_when_it_fails() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "My-Server".to_string(),
            },
            None,
            event_tx,
        )
        .expect("start the MCP action worker");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::Notice(notice)) if notice == MCP_SERVERS_NOT_STARTED
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::McpActionFinished { server }) if server == "my_server"
        ));
    }

    /// A stdio MCP server that lists the tools in `<dir>/tools.json`, read
    /// again at each `tools/list`.
    #[cfg(unix)]
    fn listing_mcp_server(
        name: &str,
        dir: &Path,
        tools: &str,
    ) -> orca_core::mcp_types::McpServerConfig {
        std::fs::write(dir.join("tools.json"), tools).unwrap();
        let script = dir.join("server.sh");
        std::fs::write(
            &script,
            r#"state_dir="$1"
printf '%s\n' "$$" >> "$state_dir/pids"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"listing","version":"1"}}}\n'
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":%s}}\n' "$(cat "$state_dir/tools.json")"
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

    /// The process ids the listing fixture in `dir` started with, in order.
    #[cfg(unix)]
    fn fixture_pids(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("pids"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Waits up to five seconds for process `pid` to be gone.
    #[cfg(unix)]
    fn wait_until_reaped(pid: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "MCP fixture process {pid} is still running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_reconnect_brings_the_server_s_new_tools_to_the_panel() {
        use crate::surface_projection::SurfaceProjectionState;
        use crate::types::AppState;

        let home = crate::test_support::isolate_orca_home();
        let fixture = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = orca_core::config::HistoryMode::Record;
        config.mcp_servers = vec![listing_mcp_server(
            "docs",
            fixture.path(),
            r#"[{"name":"before","inputSchema":{"type":"object"}}]"#,
        )];
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config, "mcp panel reconnect")
            .expect("runtime thread");
        let (state_tx, _state_rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            state_tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        // What the controller's idle poll does with each new cursor.
        let project = |state: &mut AppState| {
            let snapshot = crate::surface_client::read_snapshot(&thread.typed_surface())
                .expect("thread snapshot");
            state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
                SurfaceProjectionState::from_surface_snapshot(&snapshot),
            )));
            state
                .mcp_catalog
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while project(&mut state) != ["before"] {
            assert!(std::time::Instant::now() < deadline, "no catalog");
            std::thread::sleep(Duration::from_millis(10));
        }

        std::fs::write(
            fixture.path().join("tools.json"),
            r#"[{"name":"after","inputSchema":{"type":"object"}}]"#,
        )
        .unwrap();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "docs".to_string(),
            },
            Some(thread.clone()),
            event_tx,
        )
        .expect("start the MCP action worker");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(20)),
            Ok(TuiEvent::Notice(notice)) if notice == "MCP server docs: connected"
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::McpActionFinished { server }) if server == "docs"
        ));
        assert_eq!(project(&mut state), ["after"]);
        // The reconnect replaced the first server, which is gone; shutting
        // the thread down takes the second.
        let pids = fixture_pids(fixture.path());
        assert_eq!(pids.len(), 2, "{pids:?}");
        wait_until_reaped(&pids[0]);
        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        drop(thread);
        wait_until_reaped(&pids[1]);
    }

    #[test]
    fn reconnecting_a_server_the_thread_lacks_says_so() {
        let home = crate::test_support::isolate_orca_home();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        // A thread that records its history has the typed surface the TUI
        // talks to.
        config.history_mode = orca_core::config::HistoryMode::Record;
        let host = orca_runtime::runtime_host::RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config, "mcp reconnect")
            .expect("runtime thread");
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "nope".to_string(),
            },
            Some(thread.clone()),
            event_tx,
        )
        .expect("start the MCP action worker");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(10)),
            Ok(TuiEvent::Notice(notice))
                if notice == "MCP server nope: failed: no MCP server named 'nope'"
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::McpActionFinished { server }) if server == "nope"
        ));
        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    }
}
