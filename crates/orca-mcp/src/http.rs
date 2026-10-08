//! Every HTTP client Orca builds starts here.
//!
//! reqwest is built with `rustls-no-provider`: TLS goes through rustls,
//! certificates are checked against the operating system's trust store
//! (`rustls-platform-verifier`), and the cryptography comes from the
//! provider the process installed. These builders install ring, once,
//! before the first client.
//!
//! A client can fail to build. On Linux the system's CA certificates are
//! read when a client is built, so a system that has none (a minimal
//! container image) cannot build one at all. That is an error, never a
//! panic: [`client`] and [`blocking_client`] return it, and [`build_error`]
//! words it for the callers that use the builders.

use std::sync::Once;

/// What the platform verifier says when it finds no CA certificate on the
/// system. The Linux integration test of this crate fails if a new version
/// of it says something else.
const NO_CA_CERTIFICATES: &str = "No CA certificates were loaded from the system";

/// A builder for an async client.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}

/// A builder for a blocking client.
pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    install_crypto_provider();
    reqwest::blocking::Client::builder()
}

/// An async client with reqwest's defaults, as `reqwest::Client::new()`
/// builds it, but an error where that panics. The error is worded by
/// [`build_error`].
pub fn client() -> Result<reqwest::Client, String> {
    client_builder()
        .build()
        .map_err(|error| build_error(&error))
}

/// A blocking client with reqwest's defaults, as
/// `reqwest::blocking::Client::new()` builds it, but an error where that
/// panics. The error is worded by [`build_error`].
pub fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    blocking_client_builder()
        .build()
        .map_err(|error| build_error(&error))
}

/// Why a client could not be built, for a message to the user: reqwest's
/// error with each cause after it, so the reason survives ("builder error:
/// unexpected error: No CA certificates were loaded from the system"). On
/// Linux, when the reason is that the system has no CA certificates, it
/// adds what to install.
pub fn build_error(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    with_install_hint(text, cfg!(target_os = "linux"))
}

/// `text` with what to install when it says the system has no CA
/// certificates, on Linux. The package is named for Linux systems only: the
/// other platforms have a store of their own, which is never empty.
fn with_install_hint(mut text: String, linux: bool) -> String {
    if linux && text.contains(NO_CA_CERTIFICATES) {
        text.push_str("; install the ca-certificates package");
    }
    text
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Another library may have installed a provider first; reqwest
        // then uses that one, which is as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_error_keeps_the_cause_reqwest_gives() {
        // No TLS version is both at least 1.3 and at most 1.2, on every
        // platform, so these builds fail without touching the system.
        let from_async = client_builder()
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .max_tls_version(reqwest::tls::Version::TLS_1_2)
            .build()
            .expect_err("no TLS version satisfies both bounds");
        let from_blocking = blocking_client_builder()
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .max_tls_version(reqwest::tls::Version::TLS_1_2)
            .build()
            .expect_err("no TLS version satisfies both bounds");

        for error in [from_async, from_blocking] {
            assert_eq!(
                build_error(&error),
                "builder error: empty supported tls versions"
            );
        }
    }

    #[test]
    fn the_install_hint_is_for_linux_and_for_a_system_without_ca_certificates() {
        let no_certificates =
            "builder error: unexpected error: No CA certificates were loaded from the system";

        assert_eq!(
            with_install_hint(no_certificates.to_string(), true),
            format!("{no_certificates}; install the ca-certificates package")
        );
        assert_eq!(
            with_install_hint(no_certificates.to_string(), false),
            no_certificates
        );
        assert_eq!(
            with_install_hint("builder error: invalid proxy".to_string(), true),
            "builder error: invalid proxy"
        );
    }
}
