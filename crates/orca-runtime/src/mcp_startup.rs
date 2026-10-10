//! What a session's MCP servers leave to report once their startup ends:
//! the warnings `orca exec` prints, and the TUI shows, for servers that did
//! not start as configured.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use orca_mcp::{McpChangeSubscription, McpRegistry, McpServerState};

/// The warnings `registry` leaves once its servers' startup has ended, in
/// its words: what is wrong with the config, and then, server by server in
/// config order, why one failed to start, the login one needs, or what one
/// that started left out. A failure that does not name its server, as "MCP
/// server closed stdout" does not, is prefixed with the server's name; the
/// login a server needs names it in the `orca mcp login` it asks for.
///
/// The servers are read as they stood when their startup ended
/// ([`McpRegistry::startup_statuses`]), however long ago, not as they stand:
/// a reconnect that failed since was told to whoever asked for it, in the
/// words of `/mcp`, and is no failure of startup. Called before the startup
/// has ended, it reads them as they stand.
pub fn mcp_startup_warnings(registry: &McpRegistry) -> Vec<String> {
    let mut warnings = registry.config_errors();
    let statuses = registry
        .startup_statuses()
        .unwrap_or_else(|| registry.server_statuses());
    for server in statuses {
        match server.state {
            McpServerState::Failed { message } => {
                let named = message.contains(&format!("'{}'", server.name));
                warnings.push(if named {
                    message
                } else {
                    format!("MCP server '{}': {message}", server.name)
                });
            }
            // The login it needs, or what it left out when it started.
            _ => warnings.extend(server.errors),
        }
    }
    warnings
}

/// Who waits for the warnings a startup leaves.
type WarningsReport = Box<dyn FnOnce(Vec<String>) + Send>;

/// The warnings a session's MCP servers leave once their startup has ended
/// ([`mcp_startup_warnings`]), taken once, when it ends: what happens to
/// the servers since, such as a reconnect that fails, does not change them.
/// So a surface that shows the thread again hears the same ones, and so
/// does a thread lent servers that started before it, whose startup may
/// have ended already: it hears how that ended, not how they stand when it
/// is lent them.
pub(crate) struct McpStartupWarnings {
    state: Mutex<StartupState>,
}

enum StartupState {
    /// A server is still starting. The reports wait for the warnings, and
    /// the subscription watches the registry until then.
    Starting {
        reports: Vec<WarningsReport>,
        subscription: Option<McpChangeSubscription>,
    },
    /// Startup ended, and left these.
    Ended(Vec<String>),
}

impl McpStartupWarnings {
    /// Watches `registry` until none of its servers is starting, and takes
    /// the warnings then, at once if none is now. The registry is held only
    /// weakly meanwhile, so the watch keeps no server running.
    pub(crate) fn watch(registry: &McpRegistry) -> Arc<Self> {
        let watch = Arc::new(Self {
            state: Mutex::new(StartupState::Starting {
                reports: Vec::new(),
                subscription: None,
            }),
        });
        let watcher = Arc::downgrade(&watch);
        let subscription = registry.subscribe(Arc::new(move |registry: &McpRegistry| {
            if let Some(watch) = watcher.upgrade() {
                watch.observe(registry);
            }
        }));
        let ended = match &mut *watch.lock() {
            StartupState::Starting {
                subscription: watching,
                ..
            } => {
                *watching = Some(subscription);
                None
            }
            // It ended meanwhile, and the subscription with it.
            StartupState::Ended(_) => Some(subscription),
        };
        drop(ended);
        // Startup may have ended before the subscription began.
        watch.observe(registry);
        watch
    }

    fn lock(&self) -> MutexGuard<'_, StartupState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the warnings, once, should `registry`'s startup have ended,
    /// and hands them to each report waiting for them.
    fn observe(&self, registry: &McpRegistry) {
        // Every first connection must have ended; a reconnect since is no
        // part of the startup, and does not hold up its warnings.
        if registry.startup_statuses().is_none() {
            return;
        }
        let (reports, subscription, warnings) = {
            let mut state = self.lock();
            let StartupState::Starting {
                reports,
                subscription,
            } = &mut *state
            else {
                return;
            };
            let reports = std::mem::take(reports);
            let subscription = subscription.take();
            let warnings = mcp_startup_warnings(registry);
            *state = StartupState::Ended(warnings.clone());
            (reports, subscription, warnings)
        };
        // Startup is over, so the subscription ends, whichever call this is.
        drop(subscription);
        for report in reports {
            report(warnings.clone());
        }
    }

    /// Calls `report` with the warnings once the startup of `registry`, the
    /// one watched, has ended, at once if it has. `report` is called only
    /// once.
    pub(crate) fn on_ended(
        &self,
        registry: &McpRegistry,
        report: impl FnOnce(Vec<String>) + Send + 'static,
    ) {
        // The change that ended startup may not have been heard yet.
        self.observe(registry);
        let mut state = self.lock();
        match &mut *state {
            StartupState::Starting { reports, .. } => reports.push(Box::new(report)),
            StartupState::Ended(warnings) => {
                let warnings = warnings.clone();
                drop(state);
                report(warnings);
            }
        }
    }

    /// The warnings, for a caller that has waited for `registry`'s startup
    /// to end: those taken when it ended, or, should a server have started
    /// connecting again before they were, those of its startup all the same
    /// ([`mcp_startup_warnings`]).
    pub(crate) fn after_startup(&self, registry: &McpRegistry) -> Vec<String> {
        self.observe(registry);
        match &*self.lock() {
            StartupState::Ended(warnings) => warnings.clone(),
            StartupState::Starting { .. } => mcp_startup_warnings(registry),
        }
    }
}

// The servers are shell scripts.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use orca_core::mcp_types::McpServerConfig;

    /// A stdio server that `/bin/sh` runs from `script`, written to `dir`.
    fn stdio_server(name: &str, dir: &std::path::Path, script: &str) -> McpServerConfig {
        let path = dir.join(format!("{name}.sh"));
        std::fs::write(&path, script).expect("write the MCP fixture");
        McpServerConfig {
            name: name.to_string(),
            command: Some("/bin/sh".to_string()),
            args: vec![path.to_string_lossy().into_owned()],
            startup_timeout_ms: Some(15_000),
            ..Default::default()
        }
    }

    /// A stdio server reconnected after its first connection ended is
    /// starting again, but its startup is over: the warnings it left come at
    /// once, not when that reconnect ends.
    #[cfg(unix)]
    #[test]
    fn a_reconnect_after_startup_does_not_hold_up_its_warnings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let generation = dir.path().join("generation");
        // The first start answers at once; any later one takes 30 s first.
        let server = stdio_server(
            "again",
            dir.path(),
            &format!(
                r#"#!/bin/sh
if [ -f "{generation}" ]; then
  sleep 30
fi
printf 1 > "{generation}"
while IFS= read -r line; do
  id=${{line#*'"id":'}}
  id=${{id%%,*}}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}},"serverInfo":{{"name":"again","version":"1"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[]}}}}\n' "$id"
      ;;
  esac
done
"#,
                generation = generation.display()
            ),
        );
        let registry = orca_mcp::initialize_registry(&[server], None);
        assert!(registry.wait_for_startup(&|| false));
        let reconnecting = registry.clone();
        let reconnect = std::thread::spawn(move || {
            let _ = reconnecting.reconnect_server("again");
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !registry.is_starting() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reconnect never began"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let (report_tx, report_rx) = std::sync::mpsc::channel();
        let watch = McpStartupWarnings::watch(&registry);
        watch.on_ended(&registry, move |warnings| {
            let _ = report_tx.send(warnings);
        });

        assert_eq!(
            report_rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(Vec::new()),
            "the warnings waited for the reconnect"
        );
        registry.close();
        let _ = reconnect.join();
    }

    #[test]
    fn startup_warnings_list_the_config_then_each_server_in_order() {
        use orca_mcp::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

        let dir = tempfile::tempdir().expect("temp dir");
        // It lists its one tool twice, so it starts but leaves one out.
        let twice = stdio_server(
            "twice",
            dir.path(),
            r#"#!/bin/sh
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"twice","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}},{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#,
        );
        // It reads `initialize` and exits without answering.
        let broken = stdio_server("broken", dir.path(), "#!/bin/sh\nread -r line\nexit 0\n");
        let unnamed = McpServerConfig {
            command: Some("/bin/sh".to_string()),
            ..Default::default()
        };
        let gone = McpServerConfig {
            name: "gone".to_string(),
            command: Some(
                dir.path()
                    .join("missing-server")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        };
        // It wants a login, and none is stored.
        let remote = OAuthTestServer::start(OAuthTestBehavior::default());
        let registry = orca_mcp::initialize_registry(
            &[twice, broken, unnamed, gone, remote.config("remote")],
            None,
        );
        assert!(registry.wait_for_startup(&|| false));

        let warnings = mcp_startup_warnings(&registry);

        assert!(
            matches!(
                warnings.as_slice(),
                [unnamed, twice, broken, gone, remote]
                    if unnamed == "skipping MCP server with empty name"
                        && twice == "MCP tool name conflict: 'mcp__twice__echo' already registered, skipping from 'twice'"
                        && broken == "MCP server 'broken': MCP server closed stdout"
                        && gone.starts_with("failed to start MCP server 'gone': ")
                        && remote
                            == "MCP server requires login: run 'orca mcp login remote', or log in from /mcp"
            ),
            "{warnings:?}"
        );
    }

    /// A stdio server that serves its first start, and exits at once from
    /// every start after it, as one does that cannot start again.
    fn server_that_cannot_start_again(name: &str, dir: &std::path::Path) -> McpServerConfig {
        stdio_server(
            name,
            dir,
            r#"#!/bin/sh
if [ -e "$0.ran" ]; then
  read -r line
  exit 0
fi
: > "$0.ran"
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"docs","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#,
        )
    }

    #[test]
    fn warnings_after_a_hand_off_leave_out_a_failure_already_reported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let registry = orca_mcp::initialize_registry(
            &[server_that_cannot_start_again("docs", dir.path())],
            None,
        );
        assert!(registry.wait_for_startup(&|| false));
        assert_eq!(mcp_startup_warnings(&registry), Vec::<String>::new());

        // A reconnect after startup fails. Whoever asked for it was told
        // so, in the words of `/mcp`.
        assert!(registry.reconnect_server("docs").is_err());
        assert!(
            matches!(
                registry.server_statuses().as_slice(),
                [docs] if matches!(docs.state, McpServerState::Failed { .. })
            ),
            "{:?}",
            registry.server_statuses()
        );

        // The registry is handed to a thread, which watches it from then
        // on: what its startup left was nothing.
        let watch = McpStartupWarnings::watch(&registry);
        let (reported, warnings) = std::sync::mpsc::channel();
        watch.on_ended(&registry, move |warnings| {
            let _ = reported.send(warnings);
        });

        assert_eq!(warnings.try_recv(), Ok(Vec::<String>::new()));
        assert_eq!(watch.after_startup(&registry), Vec::<String>::new());
    }
}
