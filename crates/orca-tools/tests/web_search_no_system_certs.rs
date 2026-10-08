//! A Linux system with no CA certificates cannot build the HTTP client a web
//! search is sent with. The tool has to fail with a result that says why,
//! not panic. A process of its own, since it points the variables the CA
//! certificates are read from at nothing.

#![cfg(target_os = "linux")]

use orca_core::approval_types::ActionKind;
use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};

/// What the platform verifier says when it finds no CA certificate.
const REASON: &str = "No CA certificates were loaded from the system";
const HINT: &str = "install the ca-certificates package";

#[test]
fn a_system_without_ca_certificates_fails_the_search_with_the_reason() {
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
    let request = ToolRequest {
        id: "search-1".to_string(),
        name: ToolName::WebSearch,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(r#"{"query":"orca"}"#.to_string()),
    };

    let result = orca_tools::web_search::execute(&request, 4096);

    assert_eq!(result.status, ToolStatus::Failed);
    let error = result.error.as_deref().expect("the failure says why");
    assert!(
        error.starts_with("failed to build web search client: "),
        "unexpected error: {error}"
    );
    assert!(
        error.contains(REASON),
        "the reason is missing from: {error}"
    );
    assert!(error.contains(HINT), "the hint is missing from: {error}");
}
