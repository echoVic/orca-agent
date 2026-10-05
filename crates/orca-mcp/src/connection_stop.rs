use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::transport::McpTransport;

/// Stops the server of one connection, when it is a stdio server (see
/// [`McpTransport::terminate`]): that of the attempt that makes the
/// connection, then that of the client the attempt becomes, through each
/// transport the client reconnects to. A stopped connection starts no
/// transport again, so once [`McpConnectionStop::stop`] returns, none of
/// its servers runs.
#[derive(Default)]
pub(crate) struct McpConnectionStop {
    state: Mutex<McpConnectionStopState>,
}

#[derive(Default)]
struct McpConnectionStopState {
    stopped: bool,
    /// The transport started last. It is held weakly: its holder stops its
    /// server by dropping it.
    transport: Option<Weak<dyn McpTransport>>,
}

impl McpConnectionStop {
    /// Starts a transport to `server_name` with `start`, unless the
    /// connection was stopped. The lock is held while it starts, so that a
    /// stop waits for the transport and then stops it too.
    pub(crate) fn start(
        &self,
        server_name: &str,
        start: impl FnOnce() -> Result<Box<dyn McpTransport>, String>,
    ) -> Result<Arc<dyn McpTransport>, String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return Err(format!("MCP server '{server_name}' was stopped"));
        }
        let transport = Arc::<dyn McpTransport>::from(start()?);
        state.transport = Some(Arc::downgrade(&transport));
        Ok(transport)
    }

    /// Stops the server of the transport started last at once, even while
    /// a request to it is under way, which then fails, and keeps the
    /// connection from starting another.
    pub(crate) fn stop(&self) {
        let transport = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.stopped = true;
            state.transport.take()
        };
        if let Some(transport) = transport.and_then(|transport| transport.upgrade()) {
            transport.terminate();
        }
    }
}
