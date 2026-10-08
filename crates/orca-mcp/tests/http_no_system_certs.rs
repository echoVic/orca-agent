//! A Linux system with no CA certificates cannot check any HTTPS
//! certificate, so no HTTP client can be built on it. Whichever way Orca
//! builds one, that has to be an error that keeps the reason and names what
//! to install, never a panic. A process of its own, since it points the
//! variables the CA certificates are read from at nothing.

#![cfg(target_os = "linux")]

use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
use orca_mcp::McpServerState;
use orca_mcp::oauth::McpLoginOptions;

/// What the platform verifier says when it finds no CA certificate.
const REASON: &str = "No CA certificates were loaded from the system";
const HINT: &str = "install the ca-certificates package";

fn assert_reason_and_hint(text: &str) {
    assert!(text.contains(REASON), "the reason is missing from: {text}");
    assert!(text.contains(HINT), "the hint is missing from: {text}");
}

#[test]
fn a_system_without_ca_certificates_is_an_error_with_a_hint_not_a_panic() {
    // `SSL_CERT_FILE` and `SSL_CERT_DIR` replace the system's locations, so
    // an empty file and an empty directory leave no CA certificate at all.
    let dir = tempfile::tempdir().expect("a temp dir");
    let empty_file = dir.path().join("empty.pem");
    std::fs::write(&empty_file, "").expect("an empty CA file");
    let empty_dir = dir.path().join("certs");
    std::fs::create_dir(&empty_dir).expect("an empty CA dir");
    // SAFETY: this test binary runs this one test, and nothing in it has
    // read the environment before this point.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &empty_file);
        std::env::set_var("SSL_CERT_DIR", &empty_dir);
    }

    // The entry's two clients.
    assert_reason_and_hint(&orca_mcp::http::client().expect_err("an async client"));
    assert_reason_and_hint(&orca_mcp::http::blocking_client().expect_err("a blocking client"));

    // The builders, for the callers that set options of their own: the
    // error is reqwest's, and `build_error` words it.
    let error = orca_mcp::http::client_builder()
        .build()
        .expect_err("an async client from a builder");
    assert_reason_and_hint(&orca_mcp::http::build_error(&error));
    let error = orca_mcp::http::blocking_client_builder()
        .build()
        .expect_err("a blocking client from a builder");
    assert_reason_and_hint(&orca_mcp::http::build_error(&error));

    // An OAuth login needs a client before it reads or opens anything.
    let server = McpServerConfig {
        name: "remote-http".to_string(),
        transport: McpTransportKind::Http,
        url: Some("https://auth.example.invalid/mcp".to_string()),
        ..Default::default()
    };
    let options = McpLoginOptions::new(
        dir.path().join("mcp-credentials.json"),
        Box::new(|_| Ok(())),
    );
    let error = orca_mcp::oauth::login(&server, options).expect_err("no client, no login");
    assert!(
        error.starts_with("failed to start an HTTP client for MCP server 'remote-http': "),
        "unexpected error: {error}"
    );
    assert_reason_and_hint(&error);

    // Remote MCP servers fail to connect, as they do when the network is
    // down, and the registry says why.
    let servers = [
        ("remote-http", McpTransportKind::Http),
        ("remote-sse", McpTransportKind::Sse),
    ]
    .map(|(name, transport)| McpServerConfig {
        name: name.to_string(),
        transport,
        url: Some("http://127.0.0.1:9/mcp".to_string()),
        ..Default::default()
    });
    let registry = orca_mcp::initialize_registry(&servers, None);
    assert!(registry.wait_for_startup(&|| false));
    let statuses = registry.server_statuses();
    assert_eq!(statuses.len(), 2);
    for status in statuses {
        let McpServerState::Failed { message } = &status.state else {
            panic!(
                "{} should have failed to connect: {:?}",
                status.name, status
            );
        };
        assert_reason_and_hint(message);
    }
}
