//! The update check is a courtesy: when the system has no CA certificates,
//! no client can be built to ask the registry with, and the check has to
//! give up with an error the startup ignores, not panic. A process of its
//! own, since it points the variables the CA certificates are read from at
//! nothing.

#![cfg(target_os = "linux")]

use orca_runtime::update_check::{check_latest, check_latest_for_prompt};

/// What the platform verifier says when it finds no CA certificate.
const REASON: &str = "No CA certificates were loaded from the system";
const HINT: &str = "install the ca-certificates package";

#[test]
fn the_update_check_gives_up_without_a_panic_when_the_system_has_no_ca_certificates() {
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

    // Both checks try the npm registry, then GitHub, so both clients fail.
    for result in [check_latest("0.0.1"), check_latest_for_prompt("0.0.1")] {
        let error = result.expect_err("no client, no check");
        assert!(
            error.contains(REASON),
            "the reason is missing from: {error}"
        );
        assert!(error.contains(HINT), "the hint is missing from: {error}");
    }
}
