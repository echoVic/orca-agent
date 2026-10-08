//! The HTTP client entry in a process of its own: nothing else has
//! installed a rustls crypto provider here, so building a client works
//! only because the entry installs one.

#[test]
fn a_fresh_process_builds_clients_through_the_entry() {
    orca_mcp::http::client_builder()
        .build()
        .expect("async client");
    orca_mcp::http::blocking_client_builder()
        .build()
        .expect("blocking client");
    let _ = orca_mcp::http::client();
    let _ = orca_mcp::http::blocking_client();
}
