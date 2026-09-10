//! ACP (Agent Client Protocol) adapter layer.
//!
//! Provides `orca --mode=acp` as a bounded protocol adapter over the same
//! runtime-owned typed surface used by the TUI.

mod agent;
pub mod client;
pub mod daemon;
mod observer;
#[allow(dead_code)]
pub(crate) mod rpc_facade;
mod settings;
mod shared;
mod supervisor;
mod transport;

pub use agent::OrcaAcpAgent;
pub use observer::PROJECTION_META;

use orca_core::config::RunConfig;

use crate::surface::RuntimeSurfaceHostHandle;

/// Runs the ACP adapter on stdio using a caller-owned runtime surface host.
pub fn run_with_surface_host(surface_host: RuntimeSurfaceHostHandle, config: RunConfig) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("orca: failed to build tokio runtime: {e}");
            return 1;
        }
    };

    let local_set = tokio::task::LocalSet::new();
    let exit_code = local_set.block_on(&rt, async {
        let (incoming, outgoing) = transport::stdio();
        match supervisor::run_connection(surface_host, config, incoming, outgoing).await {
            Ok(()) => 0,
            Err(_) => 1,
        }
    });

    exit_code
}
