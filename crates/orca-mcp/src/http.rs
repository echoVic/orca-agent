//! Every HTTP client Orca builds starts here.
//!
//! reqwest is built with `rustls-no-provider`: TLS goes through rustls,
//! certificates are checked against the operating system's trust store
//! (`rustls-platform-verifier`), and the cryptography comes from the
//! provider the process installed. These builders install ring, once,
//! before the first client.

use std::sync::Once;

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

/// An async client with reqwest's defaults, as `reqwest::Client::new()`.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("failed to build HTTP client")
}

/// A blocking client with reqwest's defaults, as
/// `reqwest::blocking::Client::new()`.
pub fn blocking_client() -> reqwest::blocking::Client {
    blocking_client_builder()
        .build()
        .expect("failed to build blocking HTTP client")
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Another library may have installed a provider first; reqwest
        // then uses that one, which is as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
