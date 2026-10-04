//! What a session's MCP servers leave to report once their startup ends:
//! the warnings `orca exec` prints, and the TUI shows, for servers that did
//! not start as configured.

use std::sync::{Arc, Mutex};

use orca_mcp::{McpChangeSubscription, McpRegistry, McpServerState};

/// The warnings `registry` leaves once its servers' startup has ended, in
/// its words: what is wrong with the config, and then, server by server in
/// config order, why one failed to start, the login one needs, or what one
/// that started left out. A failure that does not name its server, as "MCP
/// server closed stdout" does not, is prefixed with the server's name; the
/// login a server needs names it in the `orca mcp login` it asks for.
pub(crate) fn mcp_startup_warnings(registry: &McpRegistry) -> Vec<String> {
    let mut warnings = registry.config_errors();
    for server in registry.server_statuses() {
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

/// Calls `report` with [`mcp_startup_warnings`] once none of `registry`'s
/// servers is still making its first connection, at once if none is.
/// `report` is called only once. The registry is held only weakly meanwhile,
/// so the wait keeps no server running.
pub(crate) fn report_mcp_startup_warnings(
    registry: &McpRegistry,
    report: impl FnOnce(Vec<String>) + Send + 'static,
) {
    let report = Mutex::new(Some(report));
    let subscription = Arc::new(Mutex::new(None::<McpChangeSubscription>));
    let report_once = {
        let subscription = Arc::clone(&subscription);
        Arc::new(move |registry: &McpRegistry| {
            if registry.is_starting() {
                return;
            }
            let report = report
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(report) = report {
                report(mcp_startup_warnings(registry));
            }
            // Startup is over, so the subscription ends, whichever call
            // this is.
            let ended = subscription
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            drop(ended);
        })
    };
    let subscribed = registry.subscribe(report_once.clone());
    *subscription
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(subscribed);
    // Startup may have ended before the subscription began.
    report_once(registry);
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
}
