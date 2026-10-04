//! What `/mcp`'s reconnect, log in and log out do. Each runs on a worker of
//! its own, off the UI thread and the action dispatcher: a reconnect takes
//! up to the server's startup timeout, and a login waits for the browser
//! for up to five minutes. Each says how it went in notices, and ends with
//! `McpActionFinished`, which lets `/mcp` act on the server again. They act
//! on the MCP servers in view when they get to them: before the first
//! message, those that started with the TUI; after, the thread's, which are
//! the same ones once the thread has taken them.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Sender;
use orca_core::config::mcp_credentials::delete_mcp_credential;
use orca_core::mcp_types::McpServerConfig;
use orca_mcp::oauth::{McpLoginOptions, OpenBrowser};
use orca_runtime::runtime_host::RuntimeThreadHandle;

use crate::prestart_mcp::McpServers;
use crate::protocol::{TuiEvent, UserAction};
use crate::surface_projection::{McpCatalogView, McpServerStatusView};

const NO_CONFIG_DIR: &str = "could not resolve the Orca configuration directory";

/// Why there is nothing to reconnect: no thread, and no MCP servers that
/// started with the TUI, as while a thread is taking them over.
const MCP_SERVERS_UNAVAILABLE: &str = "the conversation is unavailable";

/// A `/mcp` action whose worker could not start. Nothing else frees its
/// server, so the caller must deliver `McpActionFinished` for it.
#[derive(Debug)]
pub(crate) struct McpActionNotStarted {
    /// The catalog's name for the server.
    pub(crate) server: String,
    /// Why, for the user.
    pub(crate) notice: String,
}

/// Starts `action`, a `McpReconnect`, `McpLogin` or `McpLogout`, on a
/// worker of its own, on the MCP servers `servers` gives when the action
/// gets to them, saving and deleting logins in `credentials_path`. Closing
/// `/mcp` does not stop it.
pub(crate) fn spawn_mcp_server_action(
    action: UserAction,
    servers: impl Fn() -> Option<McpServers> + Send + 'static,
    credentials_path: Option<PathBuf>,
    event_tx: Sender<TuiEvent>,
) -> Result<(), McpActionNotStarted> {
    let Some(name) = action_server_name(&action).map(str::to_string) else {
        return Ok(());
    };
    std::thread::Builder::new()
        .name("orca-tui-mcp-action".to_string())
        .spawn(move || {
            run_mcp_server_worker(
                action,
                &servers,
                credentials_path.as_deref(),
                &event_tx,
                |server, cancel| {
                    log_in_with_browser(server, cancel, credentials_path.as_deref(), &event_tx)
                },
            );
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
        UserAction::McpLogin { server, .. } => Some(&server.name),
        _ => None,
    }
}

/// A worker's whole run: `action`, then `McpActionFinished` for its server,
/// however the action ends, a panic included.
fn run_mcp_server_worker(
    action: UserAction,
    servers: &dyn Fn() -> Option<McpServers>,
    credentials_path: Option<&Path>,
    event_tx: &Sender<TuiEvent>,
    log_in: impl FnOnce(&McpServerConfig, &Arc<AtomicBool>) -> Result<(), String>,
) {
    let Some(name) = action_server_name(&action) else {
        return;
    };
    let _finished = FinishOnDrop {
        event_tx: event_tx.clone(),
        server: orca_mcp::canonical_server_name(name),
    };
    run_mcp_server_action(action, servers, credentials_path, event_tx, log_in);
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

/// Runs `action` on the MCP servers `servers` gives when it gets to them,
/// logging in with `log_in`, which a login's cancel flag stops, and deleting
/// logins from `credentials_path`.
fn run_mcp_server_action(
    action: UserAction,
    servers: &dyn Fn() -> Option<McpServers>,
    credentials_path: Option<&Path>,
    event_tx: &Sender<TuiEvent>,
    log_in: impl FnOnce(&McpServerConfig, &Arc<AtomicBool>) -> Result<(), String>,
) {
    let notice = |text: String| {
        let _ = event_tx.send(TuiEvent::Notice(text));
    };
    // Asked for only now: a login started before the first message may end
    // after it, when the thread has the servers.
    let reconnect = |server: &str| reconnect_on(servers(), server);
    match action {
        UserAction::McpReconnect { server } => notice(reconnect(&server)),
        UserAction::McpLogin { server, cancel } => log_in_then_reconnect(
            &server.name,
            || log_in(&server, &cancel),
            || reconnect(&server.name),
            &cancel,
            &notice,
        ),
        UserAction::McpLogout { server } => {
            log_out_then_reconnect(&server, credentials_path, || reconnect(&server), &notice)
        }
        _ => {}
    }
}

/// Reconnects `server` (its config or catalog name) on `servers`, and says
/// what that left it as.
fn reconnect_on(servers: Option<McpServers>, server: &str) -> String {
    match servers {
        Some(McpServers::Thread(thread)) => reconnect_on_thread(&thread, server),
        Some(McpServers::Prestarted(registry)) => reconnect_prestarted(&registry, server),
        None => reconnect_failed(server, MCP_SERVERS_UNAVAILABLE),
    }
}

/// Reconnects `server` on the MCP servers of `thread`, through its typed
/// surface, and says what that left it as.
fn reconnect_on_thread(thread: &RuntimeThreadHandle, server: &str) -> String {
    let surface = thread.typed_surface();
    match crate::surface_client::reconnect_mcp_server(&surface, server) {
        Ok(status) => {
            let status = McpServerStatusView::from_surface(&status);
            // The catalog the reconnect published counts the server's tools.
            let tools = (status == McpServerStatusView::Connected)
                .then(|| crate::surface_client::read_snapshot(&surface).ok())
                .flatten()
                .map(|snapshot| {
                    McpCatalogView::from_surface(&snapshot.mcp_catalog)
                        .server_tools(&orca_mcp::canonical_server_name(server))
                        .count()
                });
            status_notice(server, &status, tools)
        }
        Err(error) => reconnect_failed(server, &error),
    }
}

/// Reconnects `server` (its config or catalog name) on the MCP servers that
/// started with the TUI, and says what that left it as, as the thread's
/// catalog would (see [`reconnect_notice`]).
fn reconnect_prestarted(registry: &orca_mcp::McpRegistry, server: &str) -> String {
    let result = registry.reconnect_server(server);
    let name = orca_mcp::canonical_server_name(server);
    // A reconnect a newer one overtook fails with what its stopped attempt
    // hit, such as "MCP server closed stdout": the server is the newer one's
    // to connect, and how it ends up is waited for. Each request of that
    // attempt is bounded by the server's startup timeout.
    if result.is_err() {
        while registry
            .server_statuses()
            .iter()
            .any(|status| status.name == name && status.state == orca_mcp::McpServerState::Starting)
        {
            std::thread::sleep(OVERTAKEN_RECONNECT_POLL);
        }
    }
    reconnect_notice(server, &McpCatalogView::from_registry(registry), result)
}

/// What a reconnect of `server` (its config or catalog name) that ended
/// with `result` says, `catalog` being how the servers stand after it: the
/// status the catalog gives the server, with its tools. For a server the
/// catalog does not list, such as a name no server has, it is what the
/// reconnect itself came to.
fn reconnect_notice(server: &str, catalog: &McpCatalogView, result: Result<(), String>) -> String {
    let name = orca_mcp::canonical_server_name(server);
    match catalog.servers.iter().find(|listed| listed.name == name) {
        Some(listed) => status_notice(
            server,
            &listed.status,
            Some(catalog.server_tools(&name).count()),
        ),
        None => {
            let status = match result {
                Ok(()) => McpServerStatusView::Connected,
                Err(error) => McpServerStatusView::Failed(error),
            };
            status_notice(server, &status, None)
        }
    }
}

/// How often a reconnect a newer one overtook looks at how the server
/// stands.
const OVERTAKEN_RECONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Logs in to `server` in the browser, saving the tokens in
/// `credentials_path` under its config name, as `orca mcp login` does,
/// until `cancel` is set: a login waiting for the browser then stops, and
/// frees its callback's port.
fn log_in_with_browser(
    server: &McpServerConfig,
    cancel: &Arc<AtomicBool>,
    credentials_path: Option<&Path>,
    event_tx: &Sender<TuiEvent>,
) -> Result<(), String> {
    let credentials_path = credentials_path.ok_or(NO_CONFIG_DIR)?.to_path_buf();
    let open_browser = browser_opener(
        orca_mcp::canonical_server_name(&server.name),
        event_tx.clone(),
        orca_platform::process::open_url,
    );
    orca_mcp::oauth::login(
        server,
        McpLoginOptions {
            cancel: Arc::clone(cancel),
            ..McpLoginOptions::new(credentials_path, open_browser)
        },
    )
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
/// it with `reconnect`, whose notice it shows. A login that fails is shown,
/// and nothing is reconnected; one that `cancel` stopped is not, as `/mcp`
/// said so when it was cancelled.
fn log_in_then_reconnect(
    server: &str,
    login: impl FnOnce() -> Result<(), String>,
    reconnect: impl FnOnce() -> String,
    cancel: &AtomicBool,
    notice: &dyn Fn(String),
) {
    notice(format!("waiting for browser login for {server}…"));
    match login() {
        Ok(()) => {
            notice(format!("logged in to MCP server {server}"));
            notice(reconnect());
        }
        Err(_) if cancel.load(Ordering::Acquire) => {}
        Err(error) => notice(error),
    }
}

/// `o`: deletes the login saved in `credentials_path` under `server`, the
/// server's config name, and then reconnects it with `reconnect`, whose
/// notice it shows. With no login saved, it says so and reconnects nothing.
fn log_out_then_reconnect(
    server: &str,
    credentials_path: Option<&Path>,
    reconnect: impl FnOnce() -> String,
    notice: &dyn Fn(String),
) {
    let deleted = credentials_path
        .ok_or_else(|| io::Error::other(NO_CONFIG_DIR))
        .and_then(|path| delete_mcp_credential(path, server));
    match deleted {
        Ok(true) => {
            notice(format!("logged out of MCP server {server}"));
            notice(reconnect());
        }
        Ok(false) => notice(format!("MCP server '{server}' is not logged in")),
        Err(error) => notice(format!("failed to log out of MCP server {server}: {error}")),
    }
}

/// What a reconnect left `server` as, `status` with `tools` tools, in the
/// words of `/mcp`'s list. The panel's list itself changes only with the
/// catalog.
fn status_notice(server: &str, status: &McpServerStatusView, tools: Option<usize>) -> String {
    format!("MCP server {server}: {}", status.summary(tools))
}

/// What a reconnect of `server` that could not be made says, `error` being
/// why.
fn reconnect_failed(server: &str, error: &str) -> String {
    format!("failed to reconnect MCP server {server}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use orca_core::config::mcp_credentials::{
        MCP_CREDENTIALS_FILE, McpCredential, load_mcp_credential, save_mcp_credential,
    };
    use orca_runtime::surface::{DisplayText, SurfaceMcpServerStatus};

    /// Runs `step`, and returns the notices it showed, in order.
    fn notices_of(step: impl FnOnce(&dyn Fn(String))) -> Vec<String> {
        let notices = RefCell::new(Vec::new());
        step(&|text| notices.borrow_mut().push(text));
        notices.into_inner()
    }

    /// The notices among `events`, which hold nothing else but the end of
    /// the action.
    fn notices_among(events: impl IntoIterator<Item = TuiEvent>) -> Vec<String> {
        events
            .into_iter()
            .filter_map(|event| match event {
                TuiEvent::Notice(notice) => Some(notice),
                TuiEvent::McpActionFinished { .. } => None,
                other => panic!("the action sent {other:?}"),
            })
            .collect()
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
                || {
                    reconnects.set(reconnects.get() + 1);
                    "MCP server linear: connected · 2 tools".to_string()
                },
                &AtomicBool::new(false),
                notice,
            )
        });

        assert_eq!(reconnects.get(), 1);
        assert_eq!(
            notices,
            [
                "waiting for browser login for linear…",
                "logged in to MCP server linear",
                "MCP server linear: connected · 2 tools",
            ]
        );
    }

    #[test]
    fn a_failed_login_reconnects_nothing() {
        let notices = notices_of(|notice| {
            log_in_then_reconnect(
                "linear",
                || Err("timed out waiting for the browser login for MCP server 'linear'".into()),
                || -> String { panic!("a failed login reconnected the server") },
                &AtomicBool::new(false),
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
                || -> String { panic!("a logout with no login to drop reconnected the server") },
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
                || {
                    reconnects.set(reconnects.get() + 1);
                    "MCP server My-Server: needs login".to_string()
                },
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

    #[test]
    fn logins_are_saved_and_deleted_where_the_config_says() {
        // The Orca home's own credentials file has no login.
        let _home = crate::test_support::isolate_orca_home();
        let configured = tempfile::tempdir().unwrap();
        let credentials = configured.path().join(MCP_CREDENTIALS_FILE);
        let url = "https://linear.example/mcp";
        save_mcp_credential(&credentials, "linear", &credential(url)).unwrap();
        let logout = || UserAction::McpLogout {
            server: "linear".to_string(),
        };
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        run_mcp_server_worker(logout(), &|| None, Some(&credentials), &event_tx, |_, _| {
            unreachable!("a logout logs in")
        });

        assert_eq!(
            notices_among(event_rx.try_iter())
                .first()
                .map(String::as_str),
            Some("logged out of MCP server linear")
        );
        assert!(
            load_mcp_credential(&credentials, "linear", url)
                .unwrap()
                .is_none()
        );

        // A config without the path, as a test's, says so.
        run_mcp_server_worker(logout(), &|| None, None, &event_tx, |_, _| {
            unreachable!("a logout logs in")
        });
        assert_eq!(
            notices_among(event_rx.try_iter()),
            [
                "failed to log out of MCP server linear: could not resolve the Orca configuration directory"
            ]
        );
        assert_eq!(
            log_in_with_browser(
                &orca_core::mcp_types::McpServerConfig {
                    name: "linear".to_string(),
                    transport: orca_core::mcp_types::McpTransportKind::Http,
                    url: Some(url.to_string()),
                    ..Default::default()
                },
                &Arc::default(),
                None,
                &event_tx,
            ),
            Err("could not resolve the Orca configuration directory".to_string())
        );
    }

    #[test]
    fn the_controller_deletes_logins_where_the_config_says() {
        use crate::test_support::hosted_tui::{Tui, config, notices};

        // The Orca home's own credentials file has no login.
        let home = crate::test_support::isolate_orca_home();
        let configured = tempfile::tempdir().unwrap();
        let credentials = configured.path().join(MCP_CREDENTIALS_FILE);
        let url = "https://linear.example/mcp";
        save_mcp_credential(&credentials, "linear", &credential(url)).unwrap();
        let mut config = config(
            home.path(),
            // Listed, and never connected.
            vec![McpServerConfig {
                name: "linear".to_string(),
                transport: orca_core::mcp_types::McpTransportKind::Http,
                url: Some(url.to_string()),
                disabled: true,
                ..Default::default()
            }],
        );
        config.mcp_credentials_path = Some(credentials.clone());
        let mut tui = Tui::start(config);
        tui.until("the server to be listed", |state| {
            !state.mcp_catalog.servers.is_empty()
        });

        // `o`, through the dispatcher the controller gave the path.
        tui.command("/mcp");
        tui.press('o');
        tui.until("the logout to end", |state| {
            state.mcp_actions_in_flight.is_empty()
        });

        assert!(
            notices(&tui.state).contains(&"logged out of MCP server linear"),
            "{:?}",
            notices(&tui.state)
        );
        assert!(
            load_mcp_credential(&credentials, "linear", url)
                .unwrap()
                .is_none()
        );
        tui.quit();
    }

    /// `orca mcp add linear --url …`, `orca`, `/mcp`, `l`, before the first
    /// message. Returns the TUI, the login `l` sent, and what the TUI sends
    /// next.
    fn login_pressed_before_the_first_message() -> (
        crate::types::AppState,
        UserAction,
        crossbeam_channel::Receiver<UserAction>,
    ) {
        use std::sync::{Arc, Mutex};

        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        use crate::surface_projection::{McpCatalogView, McpServerView};

        let (action_tx, action_rx) = crossbeam_channel::unbounded();
        let mut state = crate::types::AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        // As the servers that started with the TUI stand.
        state.update(TuiEvent::McpCatalogPrestart(McpCatalogView {
            servers: vec![McpServerView {
                name: "linear".to_string(),
                status: McpServerStatusView::NeedsLogin,
                prompts_error: None,
            }],
            ..McpCatalogView::default()
        }));
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
        assert!(matches!(&action, UserAction::McpLogin { server, .. } if server.name == "linear"));
        assert!(state.mcp_actions_in_flight.contains_key("linear"));
        (state, action, action_rx)
    }

    /// The `/mcp` panel `state` shows, at 100x30, a row per line.
    fn panel_rows(state: &mut crate::types::AppState) -> Vec<String> {
        crate::test_support::frame_string(state, 100, 30)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn press(state: &mut crate::types::AppState, key: char) {
        let action_tx = state.event_tx.clone();
        crate::mcp_dialog_actions::handle_mcp_dialog_key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(key),
                crossterm::event::KeyModifiers::NONE,
            ),
            state,
            &action_tx,
        );
    }

    fn last_notice(state: &crate::types::AppState) -> Option<&str> {
        match state.transcript.messages.last() {
            Some(crate::transcript_state::ChatMessage::System { text, .. }) => Some(text.as_str()),
            _ => None,
        }
    }

    #[test]
    fn pressing_l_again_cancels_a_waiting_login() {
        let (mut state, login, sent) = login_pressed_before_the_first_message();
        let UserAction::McpLogin { cancel, .. } = &login else {
            unreachable!("`l` sent a login");
        };
        let cancel = Arc::clone(cancel);
        assert!(
            panel_rows(&mut state)
                .iter()
                .any(|row| row.contains("linear")
                    && row.contains("waiting for browser login… (l to cancel)")),
            "{}",
            panel_rows(&mut state).join("\n")
        );
        // The login waits for the browser, as `orca_mcp::oauth::login` does,
        // until it is cancelled.
        let (event_tx, events) = crossbeam_channel::unbounded();
        let worker = std::thread::spawn(move || {
            run_mcp_server_worker(login, &|| None, None, &event_tx, |server, cancel| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    if std::time::Instant::now() > deadline {
                        return Err("the login was never cancelled".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(format!("login to MCP server '{}' cancelled", server.name))
            });
        });
        let waiting = events
            .recv_timeout(Duration::from_secs(5))
            .expect("the login says it waits");
        assert!(
            matches!(&waiting, TuiEvent::Notice(text) if text == "waiting for browser login for linear…"),
            "{waiting:?}"
        );
        state.update(waiting);

        // `r` and `o` still wait for it to end.
        for key in ['r', 'o'] {
            press(&mut state, key);
            assert!(sent.try_recv().is_err(), "'{key}' sent an action");
            assert_eq!(
                last_notice(&state),
                Some("an MCP action for linear is already running")
            );
        }
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));

        // `l` cancels it.
        press(&mut state, 'l');
        assert!(sent.try_recv().is_err(), "`l` started another login");
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            last_notice(&state),
            Some("login to MCP server linear cancelled")
        );
        assert!(
            panel_rows(&mut state)
                .iter()
                .any(|row| row.contains("linear") && row.contains("cancelling login…")),
            "{}",
            panel_rows(&mut state).join("\n")
        );

        // The login ends at that, and its server is freed; the conversation
        // has said all there is to say of it.
        worker.join().expect("the login worker");
        let rest = events.try_iter().collect::<Vec<_>>();
        assert!(notices_among(rest.iter().cloned()).is_empty(), "{rest:?}");
        for event in rest {
            state.update(event);
        }
        assert!(state.mcp_actions_in_flight.is_empty());
        assert_eq!(
            last_notice(&state),
            Some("login to MCP server linear cancelled")
        );

        // `l` logs in again.
        press(&mut state, 'l');
        assert!(matches!(
            sent.try_recv(),
            Ok(UserAction::McpLogin { server, cancel }) if server.name == "linear"
                && !cancel.load(std::sync::atomic::Ordering::SeqCst)
        ));
    }

    #[test]
    fn the_browser_login_stops_once_cancelled() {
        let home = tempfile::tempdir().unwrap();
        // Nothing listens there: a login that started would fail to reach it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .unwrap()
            .port();
        let server = orca_core::mcp_types::McpServerConfig {
            name: "linear".to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Http,
            url: Some(format!("http://127.0.0.1:{port}/mcp")),
            ..Default::default()
        };
        let (event_tx, events) = crossbeam_channel::unbounded();

        assert_eq!(
            log_in_with_browser(
                &server,
                &Arc::new(AtomicBool::new(true)),
                Some(&home.path().join(MCP_CREDENTIALS_FILE)),
                &event_tx,
            ),
            Err("login to MCP server 'linear' cancelled".to_string())
        );
        assert!(events.try_recv().is_err(), "the browser opened");
    }

    #[test]
    fn a_login_that_panics_still_frees_its_server() {
        let (mut state, action, _sent) = login_pressed_before_the_first_message();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let worker = std::thread::spawn(move || {
            run_mcp_server_worker(
                action,
                &|| None,
                None,
                &event_tx,
                |_, _| -> Result<(), String> { panic!("the browser login panicked") },
            )
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
    fn reconnect_results_read_as_the_server_status_they_leave() {
        let cases = [
            (
                McpServerStatusView::Connected,
                Some(3),
                "MCP server docs: connected · 3 tools",
            ),
            (
                McpServerStatusView::Connected,
                Some(1),
                "MCP server docs: connected · 1 tool",
            ),
            // When the catalog could not be read to count them.
            (
                McpServerStatusView::Connected,
                None,
                "MCP server docs: connected",
            ),
            (
                McpServerStatusView::from_surface(&SurfaceMcpServerStatus::Failed {
                    message: DisplayText::new("no MCP server named 'docs'"),
                }),
                None,
                "MCP server docs: failed: no MCP server named 'docs'",
            ),
            (
                McpServerStatusView::NeedsLogin,
                None,
                "MCP server docs: needs login",
            ),
            (
                McpServerStatusView::Disabled,
                None,
                "MCP server docs: disabled",
            ),
            (
                McpServerStatusView::Starting,
                None,
                "MCP server docs: starting",
            ),
        ];
        for (status, tools, expected) in cases {
            assert_eq!(status_notice("docs", &status, tools), expected);
        }
        assert_eq!(
            reconnect_failed("docs", "the conversation is unavailable"),
            "failed to reconnect MCP server docs: the conversation is unavailable"
        );
    }

    #[test]
    fn a_reconnect_says_how_it_went_for_a_server_the_catalog_does_not_list() {
        let unlisted = McpCatalogView::default();

        assert_eq!(
            reconnect_notice("docs", &unlisted, Ok(())),
            "MCP server docs: connected"
        );
        assert_eq!(
            reconnect_notice("docs", &unlisted, Err("no MCP server named 'docs'".into())),
            "MCP server docs: failed: no MCP server named 'docs'"
        );
    }

    #[test]
    fn an_action_ends_with_its_server_freed_even_when_it_fails() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "My-Server".to_string(),
            },
            || None,
            None,
            event_tx,
        )
        .expect("start the MCP action worker");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::Notice(notice))
                if notice == "failed to reconnect MCP server My-Server: the conversation is unavailable"
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
            r#"[{"name":"after","inputSchema":{"type":"object"}},{"name":"again","inputSchema":{"type":"object"}}]"#,
        )
        .unwrap();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let servers = McpServers::Thread(Box::new(thread.clone()));
        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "docs".to_string(),
            },
            move || Some(servers.clone()),
            None,
            event_tx,
        )
        .expect("start the MCP action worker");

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(20)),
            Ok(TuiEvent::Notice(notice)) if notice == "MCP server docs: connected · 2 tools"
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(5)),
            Ok(TuiEvent::McpActionFinished { server }) if server == "docs"
        ));
        assert_eq!(project(&mut state), ["after", "again"]);
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
        let servers = McpServers::Thread(Box::new(thread.clone()));

        spawn_mcp_server_action(
            UserAction::McpReconnect {
                server: "nope".to_string(),
            },
            move || Some(servers.clone()),
            None,
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

    #[cfg(unix)]
    #[test]
    fn a_prestart_reconnect_overtaken_says_how_the_server_ended_up() {
        use crate::test_support::hosted_tui::{
            launches, mcp_server, wait_for_launches, wait_until_gone,
        };

        let fixture = tempfile::tempdir().unwrap();
        // It answers `initialize` a second after it starts.
        let registry =
            orca_mcp::initialize_registry(&[mcp_server("docs", fixture.path(), 1)], None);
        assert!(registry.wait_for_startup(&|| false));

        let overtaken = std::thread::spawn({
            let registry = registry.clone();
            move || reconnect_prestarted(&registry, "docs")
        });
        // Once its server has started, a newer reconnect overtakes it, which
        // stops that server: it fails with what it hit there.
        wait_for_launches(fixture.path(), 2);
        assert!(registry.reconnect_server("docs").is_ok());

        assert_eq!(
            overtaken.join().expect("the overtaken reconnect"),
            "MCP server docs: connected · 1 tool"
        );
        let pids = launches(fixture.path());
        drop(registry);
        wait_until_gone(&pids, Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_login_before_the_first_message_reconnects_the_prestarted_server() {
        use crate::test_support::hosted_tui::{launches, mcp_server, wait_until_gone};

        let fixture = tempfile::tempdir().unwrap();
        // It wants a login, which it stands for by refusing to start.
        std::fs::write(fixture.path().join("refuse"), "").unwrap();
        let server = mcp_server("docs", fixture.path(), 0);
        let registry = orca_mcp::initialize_registry(std::slice::from_ref(&server), None);
        assert!(registry.wait_for_startup(&|| false));
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let servers = McpServers::Prestarted(registry.clone());

        run_mcp_server_worker(
            UserAction::McpLogin {
                server,
                cancel: Arc::default(),
            },
            &|| Some(servers.clone()),
            None,
            &event_tx,
            |_, _| std::fs::remove_file(fixture.path().join("refuse")).map_err(|e| e.to_string()),
        );

        assert_eq!(
            notices_among(event_rx.try_iter()),
            [
                "waiting for browser login for docs…",
                "logged in to MCP server docs",
                "MCP server docs: connected · 1 tool",
            ]
        );
        let pids = launches(fixture.path());
        assert_eq!(pids.len(), 2, "{pids:?}");
        drop((servers, registry));
        wait_until_gone(&pids, Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_login_that_finishes_after_the_first_message_updates_the_thread() {
        use crate::test_support::hosted_tui::{
            Tui, WAIT, config, launches, mcp_server, status_of, wait_until_gone,
        };

        let home = crate::test_support::isolate_orca_home();
        let fixture = tempfile::tempdir().unwrap();
        // It wants a login, which it stands for by refusing to start.
        std::fs::write(fixture.path().join("refuse"), "").unwrap();
        let server = mcp_server("docs", fixture.path(), 0);
        let mut tui = Tui::start(config(home.path(), vec![server.clone()]));
        tui.until("the server to fail to start", |state| {
            matches!(
                status_of(state, "docs"),
                Some(McpServerStatusView::Failed(_))
            )
        });

        // `l` before the first message: the login waits for the browser…
        let (browser_tx, browser_rx) = std::sync::mpsc::channel::<()>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let control = tui.control();
        let refuse = fixture.path().join("refuse");
        let login = std::thread::spawn(move || {
            run_mcp_server_worker(
                UserAction::McpLogin {
                    server,
                    cancel: Arc::default(),
                },
                &|| control.mcp_servers(),
                None,
                &event_tx,
                |_, _| {
                    browser_rx
                        .recv_timeout(WAIT)
                        .map_err(|_| "the browser never came back".to_string())?;
                    std::fs::remove_file(refuse).map_err(|error| error.to_string())
                },
            );
        });
        // …which comes back once the first message has started the thread.
        tui.send("hello");
        tui.until_event("the first turn to end", |event| {
            matches!(event, TuiEvent::SessionCompleted { .. })
        });
        browser_tx.send(()).expect("the login waits");
        login.join().expect("the login worker");

        assert_eq!(
            notices_among(event_rx.try_iter()),
            [
                "waiting for browser login for docs…",
                "logged in to MCP server docs",
                "MCP server docs: connected · 1 tool",
            ]
        );
        // The thread's servers are the ones the login reconnected.
        tui.thread_catalog(|catalog| {
            catalog.servers.iter().any(|listed| {
                listed.name == "docs" && listed.status == McpServerStatusView::Connected
            }) && catalog.server_tools("docs").count() == 1
        });
        let pids = launches(fixture.path());
        assert_eq!(pids.len(), 2, "{pids:?}");
        tui.quit();
        wait_until_gone(&pids, Duration::from_secs(5));
    }
}
