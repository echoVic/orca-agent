//! How Orca talks to an MCP server: the [`McpTransport`] a connection is
//! made of, chosen by the server's configured transport (stdio, streamable
//! HTTP, or legacy SSE behind it), and the JSON-RPC messages they share.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use orca_core::capability::CapabilityReceipt;
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};

use crate::auth::RemoteAuth;

mod http;
mod stdio;

pub(crate) use http::{
    HTTP_ACCEPT, MAX_SSE_RESPONSE_BYTES, configured_headers, parse_terminal_message, remote_url,
    resolve_sse_elicitation,
};
use http::{SseFallbackTransport, StreamableHttpTransport};
#[cfg(test)]
pub(crate) use stdio::MAX_STDIO_RESPONSE_LINE_BYTES;
use stdio::StdioTransport;

/// The MCP protocol version Orca asks for when it initializes a server.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP protocol versions Orca speaks, newest first. A server may answer
/// `initialize` with any of them.
pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone, Debug, PartialEq)]
pub enum McpElicitationMode {
    Form,
    Url,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpElicitationRequest {
    pub server_name: String,
    pub id: String,
    pub mode: McpElicitationMode,
    pub message: String,
    pub url: Option<String>,
    pub requested_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpElicitationResponse {
    Accept { content: Value },
    Decline,
}

impl McpElicitationResponse {
    pub fn accept(content: Value) -> Self {
        Self::Accept { content }
    }

    pub fn decline() -> Self {
        Self::Decline
    }
}

pub trait McpElicitationHandler {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String>;
}

pub trait McpTransport: Send + Sync {
    fn capability_receipt(&self) -> Option<CapabilityReceipt> {
        None
    }
    fn initialize(&self) -> Result<Value, String>;
    /// [`Self::initialize`], for a start that `should_cancel` can stop.
    fn initialize_or_cancel(&self, should_cancel: &dyn Fn() -> bool) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.initialize()
    }
    /// Sends `tools/list` for the page that starts at `cursor`, or for the
    /// first page.
    fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String>;
    /// [`Self::list_tools`], for a start that `should_cancel` can stop.
    fn list_tools_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.list_tools(cursor)
    }
    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String>;
    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        _handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.call_tool(name, arguments)
    }
    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.call_tool_with_elicitation_handler(name, arguments, handler)
    }
    /// Sends `resources/list` for the page that starts at `cursor`, or for
    /// the first page.
    fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String>;
    fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.list_resources(cursor)
    }
    /// Sends `resources/templates/list` for the page that starts at
    /// `cursor`, or for the first page.
    fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value, String>;
    fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.list_resource_templates(cursor)
    }
    fn read_resource(&self, uri: &str) -> Result<Value, String>;
    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.read_resource(uri)
    }
    /// Sends `prompts/list` for the page that starts at `cursor`, or for
    /// the first page.
    fn list_prompts(&self, _cursor: Option<&str>) -> Result<Value, String> {
        Err("prompts are not supported by this transport".to_string())
    }
    /// Sends `prompts/get` for the prompt `name`, with `arguments` (an
    /// object of strings).
    fn get_prompt(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
        Err("prompts are not supported by this transport".to_string())
    }
    /// Whether the transport shut itself down after a failed request, so no
    /// request can go through it any more. The stdio transport does this
    /// when its server stops answering, or answers something it cannot use.
    fn is_closed(&self) -> bool {
        false
    }
    /// Stops the server at once, even while a request to it is under way,
    /// which then fails. Only a stdio server, a process of Orca's own, is
    /// stopped: a remote one is left as it is.
    fn terminate(&self) {}
}

/// Connects to the server `config` describes: starts a stdio server, or
/// opens a remote one, reading a stored OAuth login from `credentials_path`.
pub(crate) fn connect_with_credentials(
    config: &McpServerConfig,
    credentials_path: Option<PathBuf>,
) -> Result<Box<dyn McpTransport>, String> {
    match config.transport {
        McpTransportKind::Stdio => Ok(Box::new(StdioTransport::start(config)?)),
        McpTransportKind::Http => Ok(Box::new(StreamableHttpTransport::new(
            config,
            remote_auth(config, credentials_path)?,
        )?)),
        McpTransportKind::Sse => Ok(Box::new(SseFallbackTransport::new(
            config,
            remote_auth(config, credentials_path)?,
        )?)),
    }
}

/// How requests to a remote server authenticate. A missing url is reported
/// first.
fn remote_auth(
    config: &McpServerConfig,
    credentials_path: Option<PathBuf>,
) -> Result<Arc<RemoteAuth>, String> {
    remote_url(config)?;
    RemoteAuth::resolve(config, credentials_path).map(Arc::new)
}

/// The `initialize` parameters: the protocol version Orca asks for, and who
/// it is.
pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "orca",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// The params of a list request: the cursor of the page asked for, when it
/// is not the first.
pub(crate) fn list_params(cursor: Option<&str>) -> Value {
    match cursor {
        Some(cursor) => json!({ "cursor": cursor }),
        None => json!({}),
    }
}

/// Checks the protocol version a server answered `initialize` with. Returns
/// it when Orca speaks it, or `None` when the server did not name one.
pub(crate) fn negotiated_protocol_version(
    server_name: &str,
    initialize_result: &Value,
) -> Result<Option<&'static str>, String> {
    let Some(version) = initialize_result
        .get("protocolVersion")
        .filter(|version| !version.is_null())
    else {
        return Ok(None);
    };
    SUPPORTED_MCP_PROTOCOL_VERSIONS
        .into_iter()
        .find(|supported| version.as_str() == Some(*supported))
        .map(Some)
        .ok_or_else(|| {
            let version = version
                .as_str()
                .map_or_else(|| version.to_string(), str::to_string);
            format!(
                "MCP server '{server_name}' requires protocol version {version}, which Orca does not support"
            )
        })
}

/// The JSON-RPC error for a request whose method Orca does not serve.
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

/// Whether `message`, from a server, is a request Orca must answer: it names
/// a method and has an id. A message that names a method but has no id is a
/// notification, which is not answered, and only one that names no method is
/// a response.
pub(crate) fn is_server_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some()
}

/// Whether `message`, from a server, is the response to Orca's request `id`.
/// A request from the server is not, even when it carries the same id: the
/// server numbers its requests on its own.
pub(crate) fn is_response_to(message: &Value, id: u64) -> bool {
    message.get("method").is_none() && message.get("id").and_then(Value::as_u64) == Some(id)
}

/// The answer to `request`, a request from the server other than
/// `elicitation/create`, which each transport answers its own way: `ping`
/// gets an empty result, and any other method -32601, since Orca serves no
/// other. Each transport sends it best effort: one that does not reach the
/// server fails no request of Orca's, which may be answered all the same.
pub(crate) fn server_request_reply(request: &Value) -> Value {
    if request.get("method").and_then(Value::as_str) == Some("ping") {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": {}
        })
    } else {
        mcp_jsonrpc_error_response(request, METHOD_NOT_FOUND, "Method not found".to_string())
    }
}

pub(crate) fn is_elicitation_create_request(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("elicitation/create")
}

/// The answer to a question from the server that no one can put to the user.
pub(crate) fn elicitation_not_supported(request: &Value) -> Value {
    mcp_jsonrpc_error_response(
        request,
        METHOD_NOT_FOUND,
        "elicitation is not supported".to_string(),
    )
}

fn mcp_elicitation_request_from_json(
    server_name: &str,
    message: &Value,
) -> Result<McpElicitationRequest, String> {
    let id = message
        .get("id")
        .map(json_rpc_id_to_string)
        .ok_or_else(|| "MCP elicitation request missing id".to_string())?;
    let params = message
        .get("params")
        .ok_or_else(|| "MCP elicitation request missing params".to_string())?
        .as_object()
        .ok_or_else(|| "MCP elicitation request params must be an object".to_string())?;
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| "MCP elicitation request missing message".to_string())?
        .to_string();
    if params
        .get("url")
        .is_some_and(|url| !url.is_null() && !url.is_string())
    {
        return Err("MCP elicitation request url must be a string".to_string());
    }
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let requested_schema = params
        .get("requestedSchema")
        .or_else(|| params.get("requested_schema"))
        .cloned();
    let mode = if url.is_some() {
        McpElicitationMode::Url
    } else {
        McpElicitationMode::Form
    };
    Ok(McpElicitationRequest {
        server_name: server_name.to_string(),
        id,
        mode,
        message,
        url,
        requested_schema,
    })
}

pub(crate) fn json_rpc_id_to_string(id: &Value) -> String {
    match id {
        Value::String(value) => value.clone(),
        _ => id.to_string(),
    }
}

fn mcp_elicitation_response_to_json(response: McpElicitationResponse) -> Value {
    match response {
        McpElicitationResponse::Accept { content } => json!({
            "action": "accept",
            "content": content
        }),
        McpElicitationResponse::Decline => json!({
            "action": "decline"
        }),
    }
}

fn mcp_elicitation_jsonrpc_response(request: &Value, response: McpElicitationResponse) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": mcp_elicitation_response_to_json(response)
    })
}

pub(crate) fn mcp_jsonrpc_error_response(request: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "error": {
            "code": code,
            "message": message
        }
    })
}

pub(crate) fn timeout_from_ms(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(30_000).max(1))
}

pub(crate) fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

/// What the tests of both transports share.
#[cfg(test)]
mod test_support {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Connects to `config` with no stored logins, so that no test reads the
    /// user's.
    pub(super) fn connect(config: &McpServerConfig) -> Result<Box<dyn McpTransport>, String> {
        connect_with_credentials(config, None)
    }

    pub(super) struct RecordingElicitationHandler {
        pub(super) response: McpElicitationResponse,
        pub(super) requests: StdMutex<Vec<McpElicitationRequest>>,
    }

    impl RecordingElicitationHandler {
        pub(super) fn new(response: McpElicitationResponse) -> Self {
            Self {
                response,
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    impl McpElicitationHandler for RecordingElicitationHandler {
        fn handle_elicitation(
            &self,
            request: McpElicitationRequest,
        ) -> Result<McpElicitationResponse, String> {
            self.requests.lock().unwrap().push(request);
            Ok(self.response.clone())
        }
    }
}
