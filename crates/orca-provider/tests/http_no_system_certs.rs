//! A Linux system with no CA certificates cannot build the HTTP clients the
//! provider sends its requests with. A turn has to end in a provider error
//! that says why, not in a panic. A process of its own, since it points the
//! variables the CA certificates are read from at nothing.

#![cfg(target_os = "linux")]

use orca_core::cancel::CancelToken;
use orca_core::config::ReasoningEffort;
use orca_core::conversation::Conversation;
use orca_provider::{ProviderConfig, deepseek_http, http_client};

/// What the platform verifier says when it finds no CA certificate.
const REASON: &str = "No CA certificates were loaded from the system";
const HINT: &str = "install the ca-certificates package";

fn config() -> ProviderConfig {
    ProviderConfig {
        api_key: Some("test-key".to_string()),
        // Nothing listens here, and nothing is sent: the client never builds.
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: None,
        reasoning_effort: ReasoningEffort::High,
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
        max_output_tokens: None,
    }
}

fn assert_provider_error(response: &orca_core::provider_types::ProviderResponse, client: &str) {
    let error = response
        .error()
        .unwrap_or_else(|| panic!("the {client} call should end in a provider error"))
        .to_string();
    assert!(
        error.starts_with(&format!(
            "DeepSeek provider error: failed to build {client}"
        )),
        "unexpected error: {error}"
    );
    assert!(
        error.contains(REASON),
        "the reason is missing from: {error}"
    );
    assert!(error.contains(HINT), "the hint is missing from: {error}");
}

#[test]
fn a_system_without_ca_certificates_is_a_provider_error_not_a_panic() {
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
    let mut conversation = Conversation::new();
    conversation.add_user("hello".to_string());

    // The shared blocking client keeps the reason and hands it to every
    // caller, the second one too.
    for _ in 0..2 {
        let error = http_client::client().expect_err("no blocking client");
        assert!(
            error.starts_with("failed to build HTTP client: "),
            "{error}"
        );
        assert!(error.contains(REASON) && error.contains(HINT), "{error}");
    }
    let error = http_client::execute_with_retry(|client| client.get("http://127.0.0.1:9/"))
        .expect_err("no request goes out");
    assert!(error.contains(REASON) && error.contains(HINT), "{error}");

    // A non-streaming turn goes through the shared blocking client.
    let response = deepseek_http::call(&conversation, &config());
    assert_provider_error(&response, "HTTP client");

    // A streaming turn builds a client of its own.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let response = runtime.block_on(deepseek_http::call_streaming_async(
        &conversation,
        &config(),
        &CancelToken::new(),
        |_| {},
    ));
    assert_provider_error(&response, "streaming HTTP client");
}
