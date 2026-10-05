use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use crate::connection_stop::McpConnectionStop;
use crate::legacy_sse::MCP_SSE_EVENT_STREAM_CLOSED;
use crate::transport::{self, McpElicitationHandler, McpTransport};
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

// `McpCallOutput` lives with the registry that returns it. This keeps its old
// path, `orca_mcp::client::McpCallOutput`, which `orca-tools` names.
pub use crate::registry::McpCallOutput;

pub(crate) struct McpClient {
    pub(crate) config: McpServerConfig,
    pub(crate) server_name: String,
    /// Where a reconnect looks for a stored OAuth login.
    pub(crate) credentials_path: Option<PathBuf>,
    pub(crate) capabilities: McpServerCapabilities,
    /// Stops its server, through whichever transport it has.
    pub(crate) stop: Arc<McpConnectionStop>,
    /// Why the last reconnect of a stdio server failed, until the registry
    /// takes it to mark the server failed: the stopped server's transport is
    /// still in place then, and no request goes through it.
    pub(crate) reconnect_failure: Mutex<Option<String>>,
    /// Held for the whole of each request, so that a reconnect puts a new
    /// transport in place between requests.
    pub(crate) transport: Mutex<Arc<dyn McpTransport>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct McpServerCapabilities {
    pub(crate) resources: bool,
    pub(crate) prompts: bool,
}

impl McpServerCapabilities {
    pub(crate) fn from_initialize_result(value: &Value) -> Self {
        let declares = |capability: &str| {
            value
                .get("capabilities")
                .and_then(|capabilities| capabilities.get(capability))
                .is_some()
        };
        Self {
            resources: declares("resources"),
            prompts: declares("prompts"),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn resource_capable_for_test() -> Self {
        Self {
            resources: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpRequestError {
    Cancelled,
    Failed(String),
}

impl McpRequestError {
    pub(crate) fn from_message(message: String) -> Self {
        if message == MCP_TOOL_CALL_CANCELLED {
            Self::Cancelled
        } else {
            Self::Failed(message)
        }
    }
}

impl fmt::Display for McpRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str(MCP_TOOL_CALL_CANCELLED),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for McpRequestError {}

impl From<String> for McpRequestError {
    fn from(message: String) -> Self {
        Self::from_message(message)
    }
}

impl McpClient {
    /// Calls the tool `name`. A failed call is never sent again, but a
    /// failure that stopped the server connects it again first, as
    /// [`Self::request`] does, so that the next call can go through.
    pub(crate) fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, String> {
        self.request_or_cancel(
            should_cancel.unwrap_or(&|| false),
            |transport| match should_cancel {
                Some(should_cancel) => transport.call_tool_with_elicitation_handler_or_cancel(
                    name,
                    arguments,
                    elicitation_handler,
                    should_cancel,
                ),
                None => transport.call_tool_with_elicitation_handler(
                    name,
                    arguments,
                    elicitation_handler,
                ),
            },
        )
        .map_err(|error| error.to_string())
    }

    pub(crate) fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.request_or_cancel(should_cancel, |transport| {
            transport.list_resources_or_cancel(cursor, should_cancel)
        })
    }

    pub(crate) fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.request_or_cancel(should_cancel, |transport| {
            transport.list_resource_templates_or_cancel(cursor, should_cancel)
        })
    }

    pub(crate) fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, McpRequestError> {
        self.request_or_cancel(should_cancel, |transport| {
            transport.read_resource_or_cancel(uri, should_cancel)
        })
    }

    pub(crate) fn list_prompts(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request(|transport| transport.list_prompts(cursor))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request(|transport| transport.get_prompt(name, arguments))
            .map_err(|error| error.to_string())
    }

    /// Sends `request` over the transport. An error that leaves the
    /// transport unusable, or after which it closed itself, reconnects it
    /// before the error is returned. A stdio server that cannot be started
    /// again leaves why in [`Self::take_reconnect_failure`].
    ///
    /// After a cancelled call, the server is only given a short time to
    /// start again, so the call returns promptly. One that takes longer is
    /// not failed for it: its transport stays closed, and the next request
    /// starts it again with its full startup timeout first.
    fn request(
        &self,
        request: impl FnOnce(&dyn McpTransport) -> Result<Value, String>,
    ) -> Result<Value, McpRequestError> {
        self.request_or_cancel(&|| false, request)
    }

    /// [`Self::request`], for a request that `should_cancel` can stop. One
    /// cancelled before it is sent, while its stdio server is started again
    /// too, is never sent, and a cancel stops a stdio server being started
    /// again after a failure, which leaves it to the next request.
    fn request_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
        request: impl FnOnce(&dyn McpTransport) -> Result<Value, String>,
    ) -> Result<Value, McpRequestError> {
        self.restart_closed_stdio_server(should_cancel)?;
        let (result, failed) = {
            let transport = self.lock_transport().map_err(McpRequestError::Failed)?;
            // Checked with the transport held, for a request that waited for
            // another one to finish.
            if should_cancel() {
                return Err(McpRequestError::Cancelled);
            }
            let result = request(transport.as_ref());
            // The transport the request failed on, and whether it closed
            // itself.
            let failed = result
                .is_err()
                .then(|| (Arc::clone(&*transport), transport.is_closed()));
            (result, failed)
        };
        match (result, failed) {
            (Err(error), Some((failed, closed)))
                if closed || should_reconnect_after_mcp_error(&self.config.transport, &error) =>
            {
                let stdio = self.config.transport == McpTransportKind::Stdio;
                let startup_timeout_cap_ms = (stdio && error == MCP_TOOL_CALL_CANCELLED)
                    .then_some(CANCELLED_STDIO_RECONNECT_TIMEOUT_MS);
                // A stdio server started with its full startup timeout stops
                // at a cancel, and is left to the next request.
                let full_restart = stdio && startup_timeout_cap_ms.is_none();
                let reconnected = self.reconnect(
                    &failed,
                    startup_timeout_cap_ms,
                    if full_restart {
                        should_cancel
                    } else {
                        &|| false
                    },
                );
                if full_restart && !(reconnected.is_err() && should_cancel()) {
                    *self.lock_reconnect_failure() = reconnected.err();
                }
                Err(McpRequestError::from_message(error))
            }
            (Ok(result), _) => Ok(result),
            (Err(error), _) => Err(McpRequestError::from_message(error)),
        }
    }

    /// Starts a stdio server again, with its full startup timeout, when its
    /// transport is closed: a reconnect cut short after a cancelled call
    /// left it so. A cancel stops the start, and leaves the server to the
    /// next request. One that cannot start leaves why in
    /// [`Self::take_reconnect_failure`], and the request fails with it.
    fn restart_closed_stdio_server(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<(), McpRequestError> {
        if self.config.transport != McpTransportKind::Stdio {
            return Ok(());
        }
        let closed = {
            let transport = self.lock_transport().map_err(McpRequestError::Failed)?;
            if !transport.is_closed() {
                return Ok(());
            }
            Arc::clone(&*transport)
        };
        if should_cancel() {
            return Err(McpRequestError::Cancelled);
        }
        match self.reconnect(&closed, None, should_cancel) {
            Ok(()) => Ok(()),
            Err(_) if should_cancel() => Err(McpRequestError::Cancelled),
            Err(error) => {
                *self.lock_reconnect_failure() = Some(error.clone());
                Err(McpRequestError::from_message(error))
            }
        }
    }

    /// Connects the server again, with its startup timeout capped at
    /// `startup_timeout_cap_ms` when given, and puts the new transport in
    /// place of `failed`, the one a request failed on. A stdio server is
    /// stopped first, and requests wait for the new one, so that a server
    /// that listens on a fixed port can start again. A remote server keeps
    /// serving through the old transport until the new one is ready.
    ///
    /// Once `failed` is no longer the current transport, another request
    /// that failed on it has reconnected the server already, and nothing is
    /// done: stopping the server it started would fail its requests.
    pub(crate) fn reconnect(
        &self,
        failed: &Arc<dyn McpTransport>,
        startup_timeout_cap_ms: Option<u64>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let mut config = self.config.clone();
        if let Some(cap_ms) = startup_timeout_cap_ms {
            config.startup_timeout_ms =
                Some(config.startup_timeout_ms.unwrap_or(cap_ms).min(cap_ms));
        }
        if config.transport == McpTransportKind::Stdio {
            let mut current = self.lock_transport()?;
            if !Arc::ptr_eq(&current, failed) {
                return Ok(());
            }
            current.terminate();
            *current = self.connect(&config, should_cancel)?;
        } else {
            if !Arc::ptr_eq(&*self.lock_transport()?, failed) {
                return Ok(());
            }
            let transport = self.connect(&config, should_cancel)?;
            let mut current = self.lock_transport()?;
            if Arc::ptr_eq(&current, failed) {
                *current = transport;
            }
        }
        Ok(())
    }

    /// Why the last reconnect of the stdio server failed, once: its server
    /// is stopped, and the client is of no more use.
    pub(crate) fn take_reconnect_failure(&self) -> Option<String> {
        self.lock_reconnect_failure().take()
    }

    pub(crate) fn lock_reconnect_failure(&self) -> MutexGuard<'_, Option<String>> {
        self.reconnect_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A new transport to the server `config` describes, started under the
    /// client's stop, once it has answered `initialize` and `tools/list`,
    /// unless `should_cancel` stops it first.
    fn connect(
        &self,
        config: &McpServerConfig,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<dyn McpTransport>, String> {
        let transport = self.stop.start(&self.server_name, || {
            transport::connect_with_credentials(config, self.credentials_path.clone())
        })?;
        transport.initialize_or_cancel(should_cancel)?;
        let _ = transport.list_tools_or_cancel(None, should_cancel)?;
        Ok(transport)
    }

    pub(crate) fn lock_transport(&self) -> Result<MutexGuard<'_, Arc<dyn McpTransport>>, String> {
        self.transport
            .lock()
            .map_err(|_| format!("MCP server '{}' transport lock poisoned", self.server_name))
    }
}

const MCP_TOOL_CALL_CANCELLED: &str = "MCP tool call cancelled";
const CANCELLED_STDIO_RECONNECT_TIMEOUT_MS: u64 = 500;

pub(crate) fn should_reconnect_after_mcp_error(transport: &McpTransportKind, error: &str) -> bool {
    error.contains("timed out")
        || (transport == &McpTransportKind::Stdio && error.contains("MCP tool call cancelled"))
        || error.contains("reader stopped")
        || error.contains("server closed stdout")
        || error.contains("failed to write MCP request")
        // An oversized stdio response stops the reader and kills the server.
        || error.contains("MCP response exceeded maximum line size")
        // The legacy SSE event stream ended; a new connection opens another.
        || error.contains(MCP_SSE_EVENT_STREAM_CLOSED)
}
