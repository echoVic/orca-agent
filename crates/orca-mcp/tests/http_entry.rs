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
    orca_mcp::http::client().expect("async client from the entry");
    orca_mcp::http::blocking_client().expect("blocking client from the entry");
}
