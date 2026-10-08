use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use orca_core::capability::{CapabilityReceipt, EnforcementState};
use orca_core::execution_broker::ExecutionBroker;
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
use orca_platform::process::ProcessJob;
use orca_platform::shell::resolve_program;

use crate::auth::{AuthAttempt, RemoteAuth, is_auth_required};
use crate::legacy_sse::LegacySseTransport;
use crate::sse::{SseDecoder, SseEvent};

const STDIO_RESPONSE_QUEUE_CAPACITY: usize = 8;
// The largest single MCP response either transport accepts: room for two
// 5 MiB tool images (base64 adds a third) plus text, and far below the 64 MiB
// session record limit a tool result is written under.
pub(crate) const MAX_STDIO_RESPONSE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_SSE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The MCP protocol version Orca asks for when it initializes a server.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// The MCP protocol versions Orca speaks, newest first. A server may answer
/// `initialize` with any of them.
pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// A streamable HTTP POST accepts both ways a server may answer it.
pub(crate) const HTTP_ACCEPT: &str = "application/json, text/event-stream";
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// How long the DELETE that ends a dropped transport's session may take.
const HTTP_SESSION_END_TIMEOUT: Duration = Duration::from_secs(2);

struct SseElicitationEnvelope {
    request: Value,
    response: tokio::sync::oneshot::Sender<Value>,
}

struct SseRequestContext {
    endpoint: String,
    /// Every header the request carries: configured, protocol, and session.
    headers: HeaderMap,
    id: u64,
    method: String,
    timeout: Duration,
}

struct SseAsyncRequest {
    client: reqwest::Client,
    context: SseRequestContext,
    params: Value,
    cancel: Arc<AtomicBool>,
    elicitation_sender: mpsc::Sender<SseElicitationEnvelope>,
}

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

struct StdioTransport {
    server_name: String,
    capability_receipt: CapabilityReceipt,
    /// The server's process, which `state` holds too. A request holds
    /// `state` until it is answered, so [`McpTransport::terminate`] stops
    /// the process through this one.
    child: Arc<Mutex<StdioChild>>,
    /// The server's stdin, which the reader thread answers the server's
    /// requests through too.
    writer: StdioWriter,
    /// Whether a request is waiting for its answer, and so can take a
    /// question from the server (see [`read_stdio_messages`]).
    in_flight: Arc<AtomicBool>,
    state: Mutex<StdioState>,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

/// The server's stdin. Requests and the reader thread both write to it, each
/// holding the lock only while it writes one line, so lines never
/// interleave. A request takes it while it holds `state`, which the reader
/// thread never takes, so the two locks cannot deadlock.
#[derive(Clone)]
struct StdioWriter(Arc<Mutex<ChildStdin>>);

impl StdioWriter {
    fn write_line(&self, message: &Value) -> Result<(), String> {
        let mut stdin = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        write_json_line(&mut stdin, message)
    }
}

/// Marks a request as waiting for its answer until dropped.
struct InFlight<'a>(&'a AtomicBool);

impl<'a> InFlight<'a> {
    fn new(in_flight: &'a AtomicBool) -> Self {
        in_flight.store(true, Ordering::Release);
        Self(in_flight)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct StdioState {
    child: Arc<Mutex<StdioChild>>,
    responses: Option<mpsc::Receiver<Result<Value, String>>>,
    reader_worker: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
}

impl StdioState {
    fn terminate(&mut self) {
        self.responses.take();
        lock_child(&self.child).terminate();
        if let Some(worker) = self.reader_worker.take() {
            let _ = worker.join();
        }
    }

    fn terminal_error<T>(&mut self, error: String) -> Result<T, String> {
        self.terminate();
        Err(error)
    }
}

impl Drop for StdioState {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct StdioChild {
    child: Option<Child>,
    process_job: ProcessJob,
}

impl StdioChild {
    fn new(child: Child, process_job: ProcessJob) -> Self {
        Self {
            child: Some(child),
            process_job,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("stdio child is available")
    }

    fn terminate(&mut self) {
        let _ = self.process_job.terminate(137);
        let Some(mut child) = self.child.take() else {
            return;
        };
        kill_child_tree(&mut child);
        let _ = child.wait();
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn lock_child(child: &Mutex<StdioChild>) -> MutexGuard<'_, StdioChild> {
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held while a stdio server is launched on macOS or iOS, so that launches
/// take turns: servers connect in parallel. There, the pipes of a new
/// process, and the one its launch reads an exec failure from, are created
/// before they are marked close-on-exec, so a server launched at that moment
/// on another thread would inherit them, and the first launch would then
/// wait until that server exits.
static STDIO_LAUNCH: Mutex<()> = Mutex::new(());

/// Waits for this stdio launch's turn, where launches must take turns, and
/// returns what holds it until dropped: on macOS and iOS (see
/// [`STDIO_LAUNCH`]). Elsewhere launches go at once, and a launch that hangs
/// holds up no other: Linux creates the pipes close-on-exec from the start
/// (`pipe2`), and Windows starts one process at a time on its own.
fn stdio_launch_turn() -> Option<MutexGuard<'static, ()>> {
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        Some(STDIO_LAUNCH.lock().unwrap_or_else(PoisonError::into_inner))
    } else {
        None
    }
}

impl StdioTransport {
    fn start(config: &McpServerConfig) -> Result<Self, String> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| format!("MCP server '{}' is missing command", config.name))?;

        let program = resolve_program(command)
            .map_or_else(|| command.into(), std::path::PathBuf::into_os_string);
        let mut child_command = Command::new(program);
        child_command
            .env_clear()
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            // Keep the platform loader/runtime contract after clearing the
            // inherited user environment. In particular, Node and cmd-based
            // integrations may require SystemRoot to initialize, while PATH
            // and user variables remain opt-in through config.env.
            for key in ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(key) {
                    child_command.env(key, value);
                }
            }
        }
        #[cfg(unix)]
        {
            child_command.process_group(0);
        }
        let cwd = std::env::current_dir()
            .map_err(|error| format!("failed to resolve MCP server cwd: {error}"))?;
        let broker = ExecutionBroker::with_backend(
            EnforcementState::Advisory,
            "mcp-user-trusted-integration",
        );
        let launched = {
            let _turn = stdio_launch_turn();
            broker.launch_user_trusted(
                child_command,
                format!("mcp:{}", config.name),
                cwd,
                config.capabilities.clone(),
            )
        }
        .map_err(|error| format!("failed to start MCP server '{}': {error:?}", config.name))?;
        let receipt = launched.receipt;
        let (child, process_job) = (launched.child, launched.process_job);
        let mut child = StdioChild::new(child, process_job);

        let stdin = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| format!("failed to open stdin for MCP server '{}'", config.name))?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| format!("failed to open stdout for MCP server '{}'", config.name))?;
        let writer = StdioWriter(Arc::new(Mutex::new(stdin)));
        let in_flight = Arc::new(AtomicBool::new(false));
        let (response_tx, responses) = mpsc::sync_channel(STDIO_RESPONSE_QUEUE_CAPACITY);
        let reader_worker = std::thread::spawn({
            let writer = writer.clone();
            let in_flight = Arc::clone(&in_flight);
            move || read_stdio_messages(stdout, response_tx, writer, in_flight)
        });

        let child = Arc::new(Mutex::new(child));
        Ok(Self {
            server_name: config.name.clone(),
            capability_receipt: receipt,
            child: Arc::clone(&child),
            writer,
            in_flight,
            state: Mutex::new(StdioState {
                child,
                responses: Some(responses),
                reader_worker: Some(reader_worker),
                next_id: 1,
            }),
            startup_timeout: timeout_from_ms(config.startup_timeout_ms),
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
        })
    }
}

impl McpTransport for StdioTransport {
    fn capability_receipt(&self) -> Option<CapabilityReceipt> {
        Some(self.capability_receipt.clone())
    }

    fn initialize(&self) -> Result<Value, String> {
        self.handshake(None)
    }

    fn initialize_or_cancel(&self, should_cancel: &dyn Fn() -> bool) -> Result<Value, String> {
        self.handshake(Some(should_cancel))
    }

    fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_tools_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            Some(should_cancel),
        )
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.call_tool_with_elicitation_handler(name, arguments, None)
    }

    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            None,
        )
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            Some(should_cancel),
        )
    }

    fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            Some(should_cancel),
        )
    }

    fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            Some(should_cancel),
        )
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
            None,
            None,
        )
    }

    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
            None,
            Some(should_cancel),
        )
    }

    fn list_prompts(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout(
            "prompts/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request_with_timeout(
            "prompts/get",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            None,
            None,
        )
    }

    fn is_closed(&self) -> bool {
        // A terminated server's responses are gone for good.
        self.state
            .lock()
            .map_or(true, |state| state.responses.is_none())
    }

    fn terminate(&self) {
        // A request under way then reads the end of the server's output.
        lock_child(&self.child).terminate();
    }
}

impl StdioTransport {
    /// Sends `initialize` and then `notifications/initialized`. A cancel
    /// stops the server.
    fn handshake(&self, should_cancel: Option<&dyn Fn() -> bool>) -> Result<Value, String> {
        let result = self.request_with_timeout(
            "initialize",
            initialize_params(),
            self.startup_timeout,
            None,
            should_cancel,
        )?;
        negotiated_protocol_version(&self.server_name, &result)?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        elicitation_handler: Option<&dyn McpElicitationHandler>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Value, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP stdio transport lock poisoned".to_string())?;
        let id = state.next_id;
        state.next_id += 1;

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        // The server may ask its question as soon as it reads the request.
        let _in_flight = InFlight::new(&self.in_flight);
        if let Err(error) = self.writer.write_line(&message) {
            return state.terminal_error(error);
        }

        let deadline = std::time::Instant::now() + timeout;
        let mut iterations = 0u32;
        loop {
            let mut response = match state.responses.as_ref() {
                Some(responses) => match try_recv_stdio_response(responses) {
                    Ok(response) => response,
                    Err(error) => return state.terminal_error(error),
                },
                None => return state.terminal_error("MCP stdio reader is unavailable".to_string()),
            };
            if response.is_none() && should_cancel.is_some_and(|should_cancel| should_cancel()) {
                response = match state.responses.as_ref() {
                    Some(responses) => match try_recv_stdio_response(responses) {
                        Ok(response) => response,
                        Err(error) => return state.terminal_error(error),
                    },
                    None => {
                        return state.terminal_error("MCP stdio reader is unavailable".to_string());
                    }
                };
                if response.is_none() {
                    return state.terminal_error("MCP tool call cancelled".to_string());
                }
            }
            if iterations >= 1000 {
                return state.terminal_error(format!(
                    "MCP request '{method}' exceeded max notification count"
                ));
            }
            if std::time::Instant::now() >= deadline {
                return state.terminal_error(format!(
                    "MCP request '{method}' timed out after {}",
                    format_duration(timeout)
                ));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = match should_cancel {
                Some(_) => remaining.min(Duration::from_millis(25)),
                None => remaining,
            };
            let response = match response {
                Some(response) => response,
                None => match state
                    .responses
                    .as_ref()
                    .map(|responses| responses.recv_timeout(wait))
                {
                    None => {
                        return state.terminal_error("MCP stdio reader is unavailable".to_string());
                    }
                    Some(Ok(Ok(response))) => response,
                    Some(Ok(Err(error))) => return state.terminal_error(error),
                    Some(Err(mpsc::RecvTimeoutError::Timeout)) => {
                        if should_cancel.is_some() {
                            continue;
                        }
                        return state.terminal_error(format!(
                            "MCP request '{method}' timed out after {}",
                            format_duration(timeout)
                        ));
                    }
                    Some(Err(mpsc::RecvTimeoutError::Disconnected)) => {
                        return state.terminal_error(
                            "MCP stdio reader stopped before returning".to_string(),
                        );
                    }
                },
            };
            iterations += 1;
            // The reader answers every request from the server but a
            // question.
            if is_server_request(&response) {
                if is_elicitation_create_request(&response) {
                    // The server waits for the answer, so one that cannot
                    // reach it ends the request.
                    if let Err(error) = handle_elicitation_create_request(
                        &self.server_name,
                        &self.writer,
                        &response,
                        elicitation_handler,
                    ) {
                        return state.terminal_error(error);
                    }
                }
                continue;
            }
            // A notification, or the response to another request.
            if !is_response_to(&response, id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP request '{method}' failed: {error}"));
            }
            return match response.get("result").cloned() {
                Some(result) => Ok(result),
                None => state.terminal_error(format!("MCP request '{method}' missing result")),
            };
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP stdio transport lock poisoned".to_string())?;
        if let Err(error) = self.writer.write_line(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        })) {
            return state.terminal_error(error);
        }
        Ok(())
    }
}

/// Reads the server's messages until its stdout ends, or until no one takes
/// them any more. A request from the server is answered here at once, best
/// effort, whether or not a request of Orca's is under way, as
/// [`server_request_reply`] says. A question (`elicitation/create`) is the
/// exception: it goes to the request under way, which may put it to the
/// user, and is turned down when none is. A notification that comes while
/// no request is under way is dropped: a request only skips notifications,
/// and these would fill `responses`, which no one empties until the next
/// request, and stop the reader before the server's next request. The rest,
/// responses and the notifications of a request under way, goes to
/// `responses`.
fn read_stdio_messages(
    stdout: ChildStdout,
    responses: mpsc::SyncSender<Result<Value, String>>,
    writer: StdioWriter,
    in_flight: Arc<AtomicBool>,
) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let message = match read_json_line(&mut stdout) {
            Ok(message) => message,
            Err(error) => {
                let _ = responses.send(Err(error));
                return;
            }
        };
        if is_server_request(&message) {
            // A reply that does not reach the server fails no request of
            // Orca's, which may be answered all the same.
            if !is_elicitation_create_request(&message) {
                let _ = writer.write_line(&server_request_reply(&message));
                continue;
            }
            if !in_flight.load(Ordering::Acquire) {
                let _ = writer.write_line(&elicitation_not_supported(&message));
                continue;
            }
        } else if message.get("method").is_some() && !in_flight.load(Ordering::Acquire) {
            // A notification, with no request under way to skip it.
            continue;
        }
        if responses.send(Ok(message)).is_err() {
            return;
        }
    }
}

fn try_recv_stdio_response(
    responses: &mpsc::Receiver<Result<Value, String>>,
) -> Result<Option<Value>, String> {
    match responses.try_recv() {
        Ok(Ok(response)) => Ok(Some(response)),
        Ok(Err(error)) => Err(error),
        Err(mpsc::TryRecvError::Empty) => Ok(None),
        Err(mpsc::TryRecvError::Disconnected) => {
            Err("MCP stdio reader stopped before returning".to_string())
        }
    }
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

fn handle_elicitation_create_request(
    server_name: &str,
    writer: &StdioWriter,
    message: &Value,
    handler: Option<&dyn McpElicitationHandler>,
) -> Result<(), String> {
    let id = message
        .get("id")
        .cloned()
        .ok_or_else(|| "MCP elicitation request missing id".to_string())?;
    let request = mcp_elicitation_request_from_json(server_name, message)?;
    let response = match handler {
        Some(handler) => handler.handle_elicitation(request),
        None => Ok(McpElicitationResponse::decline()),
    };

    let message = match response {
        Ok(response) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": mcp_elicitation_response_to_json(response)
        }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": error
            }
        }),
    };
    writer.write_line(&message)
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

fn write_json_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("failed to write MCP request: {error}"))
}

fn read_json_line<R: BufRead>(stdout: &mut R) -> Result<Value, String> {
    let mut line = Vec::new();
    loop {
        let buffer = stdout
            .fill_buf()
            .map_err(|error| format!("failed to read MCP response: {error}"))?;
        if buffer.is_empty() {
            if line.is_empty() {
                return Err("MCP server closed stdout".to_string());
            }
            break;
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(buffer.len());
        if data_len > MAX_STDIO_RESPONSE_LINE_BYTES.saturating_sub(line.len()) {
            return Err(format!(
                "MCP response exceeded maximum line size of {MAX_STDIO_RESPONSE_LINE_BYTES} bytes"
            ));
        }
        line.extend_from_slice(&buffer[..data_len]);
        stdout.consume(data_len + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }

    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    serde_json::from_slice(&line[start..end])
        .map_err(|error| format!("invalid MCP response JSON: {error}"))
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(child.id());
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    const SIGKILL: i32 = 9;
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    unsafe {
        let _ = kill(-pid, SIGKILL);
    }
}

/// Talks to a remote MCP server over streamable HTTP: each message is a POST
/// to one endpoint, and the server answers with JSON or an SSE stream.
struct StreamableHttpTransport {
    server_name: String,
    endpoint: String,
    /// The configured headers, checked once when the transport is created.
    headers: HeaderMap,
    auth: Arc<RemoteAuth>,
    session: Mutex<HttpSession>,
    next_id: Mutex<u64>,
    client: reqwest::blocking::Client,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

/// What `initialize` agreed with the server.
#[derive(Clone, Default)]
struct HttpSession {
    /// The `Mcp-Session-Id` the server assigned, when it uses sessions.
    id: Option<HeaderValue>,
    /// The protocol version the server answered with.
    protocol_version: Option<&'static str>,
}

/// Why a streamable HTTP request failed.
#[derive(Debug)]
enum HttpRequestError {
    /// The server answered with an error status, which `message` names, as
    /// in "failed with 405 Method Not Allowed".
    Status {
        status: StatusCode,
        /// Whether the request carried an `Mcp-Session-Id`.
        in_session: bool,
        message: String,
    },
    /// Any other failure, already worded for the user.
    Failed(String),
}

impl HttpRequestError {
    fn status(context: &SseRequestContext, status: StatusCode) -> Self {
        Self::Status {
            status,
            in_session: context.headers.contains_key(MCP_SESSION_ID_HEADER),
            message: format!("MCP SSE request '{}' failed with {status}", context.method),
        }
    }

    /// Whether the server turned the request's credentials away.
    fn unauthorized(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status == StatusCode::UNAUTHORIZED)
    }

    /// Whether the server has ended the session the request carried: it
    /// answers 404 to a session ID it no longer knows.
    fn ended_session(&self) -> bool {
        matches!(
            self,
            Self::Status { status, in_session: true, .. } if *status == StatusCode::NOT_FOUND
        )
    }

    fn into_message(self) -> String {
        match self {
            Self::Status { message, .. } | Self::Failed(message) => message,
        }
    }
}

impl From<String> for HttpRequestError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// The url a remote server is reached at.
pub(crate) fn remote_url(config: &McpServerConfig) -> Result<String, String> {
    config
        .url
        .clone()
        .ok_or_else(|| format!("MCP SSE server '{}' is missing url", config.name))
}

/// The headers a remote server is configured with, checked. A configured
/// session or protocol version header is left out: those are the
/// transport's to send, and a new session must start without either.
pub(crate) fn configured_headers(config: &McpServerConfig) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (name, value) in &config.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            format!(
                "MCP server '{}' has an invalid header name '{name}'",
                config.name
            )
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            format!(
                "MCP server '{}' has an invalid value for header '{name}'",
                config.name
            )
        })?;
        headers.append(header_name, header_value);
    }
    headers.remove(MCP_SESSION_ID_HEADER);
    headers.remove(MCP_PROTOCOL_VERSION_HEADER);
    Ok(headers)
}

impl StreamableHttpTransport {
    fn new(config: &McpServerConfig, auth: Arc<RemoteAuth>) -> Result<Self, String> {
        Ok(Self {
            server_name: config.name.clone(),
            endpoint: remote_url(config)?,
            headers: configured_headers(config)?,
            auth,
            session: Mutex::new(HttpSession::default()),
            next_id: Mutex::new(1),
            client: crate::http::blocking_client(),
            startup_timeout: timeout_from_ms(config.startup_timeout_ms),
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
        })
    }
}

impl McpTransport for StreamableHttpTransport {
    fn initialize(&self) -> Result<Value, String> {
        self.start_session().map_err(HttpRequestError::into_message)
    }

    fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout("tools/list", list_params(cursor), self.startup_timeout)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            None,
            &|| false,
        )
    }

    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            &|| false,
        )
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
            handler,
            should_cancel,
        )
    }

    fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout("resources/list", list_params(cursor), self.startup_timeout)
    }

    fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "resources/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            should_cancel,
        )
    }

    fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            list_params(cursor),
            self.startup_timeout,
        )
    }

    fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "resources/templates/list",
            list_params(cursor),
            self.startup_timeout,
            None,
            should_cancel,
        )
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
        )
    }

    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "resources/read",
            json!({
                "uri": uri
            }),
            self.tool_timeout,
            None,
            should_cancel,
        )
    }

    fn list_prompts(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.request_with_timeout("prompts/list", list_params(cursor), self.startup_timeout)
    }

    fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request_with_timeout(
            "prompts/get",
            json!({
                "name": name,
                "arguments": arguments
            }),
            self.tool_timeout,
        )
    }
}

impl StreamableHttpTransport {
    /// Starts a session: sends `initialize` without session headers, keeps the
    /// session ID and protocol version the server answers with, and sends
    /// `notifications/initialized` in the new session.
    ///
    /// An earlier session stays in place until the server answers. So after a
    /// failed attempt, the next request carries the ended session again, gets
    /// 404, and tries once more to start a new one.
    fn start_session(&self) -> Result<Value, HttpRequestError> {
        let (result, response_headers) = self.authorized(|auth| {
            let context = SseRequestContext {
                endpoint: self.endpoint.clone(),
                headers: self.request_headers(&HttpSession::default(), auth),
                id: self.next_request_id()?,
                method: "initialize".to_string(),
                timeout: self.startup_timeout,
            };
            request_sse_with_client(
                &self.client,
                &self.server_name,
                &context,
                initialize_params(),
            )
        })?;
        let session_id = response_headers
            .get(MCP_SESSION_ID_HEADER)
            .filter(|id| !id.is_empty())
            .cloned();
        let protocol_version = match negotiated_protocol_version(&self.server_name, &result) {
            Ok(version) => version,
            Err(error) => {
                // Keep the session, so that dropping the transport ends it.
                *self.session()? = HttpSession {
                    id: session_id,
                    protocol_version: None,
                };
                return Err(error.into());
            }
        };
        *self.session()? = HttpSession {
            id: session_id,
            protocol_version,
        };
        self.notify("notifications/initialized", json!({}), self.startup_timeout)?;
        Ok(result)
    }

    fn session(&self) -> Result<std::sync::MutexGuard<'_, HttpSession>, String> {
        self.session
            .lock()
            .map_err(|_| "MCP SSE session lock poisoned".to_string())
    }

    /// The headers for a request in the current session.
    fn session_headers(&self, auth: &AuthAttempt) -> Result<HeaderMap, String> {
        let session = self.session()?.clone();
        Ok(self.request_headers(&session, auth))
    }

    /// The configured headers and the auth, then the headers the protocol
    /// requires, which replace any configured value of the same name.
    fn request_headers(&self, session: &HttpSession, auth: &AuthAttempt) -> HeaderMap {
        let mut headers = self.headers.clone();
        auth.apply(&mut headers);
        headers.insert(ACCEPT, HeaderValue::from_static(HTTP_ACCEPT));
        if let Some(id) = &session.id {
            headers.insert(MCP_SESSION_ID_HEADER, id.clone());
        }
        if let Some(version) = session.protocol_version {
            headers.insert(
                MCP_PROTOCOL_VERSION_HEADER,
                HeaderValue::from_static(version),
            );
        }
        headers
    }

    fn notify(&self, method: &str, params: Value, timeout: Duration) -> Result<(), String> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.authorized(|auth| {
            let headers = self.session_headers(auth)?;
            let in_session = headers.contains_key(MCP_SESSION_ID_HEADER);
            let response = self
                .client
                .post(&self.endpoint)
                .headers(headers)
                .timeout(timeout)
                .json(&message)
                .send()
                .map_err(|error| {
                    if error.is_timeout() {
                        format!(
                            "MCP SSE notify '{method}' timed out after {}",
                            format_duration(timeout)
                        )
                    } else {
                        format!("MCP SSE notify '{method}' failed: {error}")
                    }
                })?;
            // Servers acknowledge a notification with 202 and no body; some send 200.
            let status = response.status();
            if !status.is_success() {
                return Err(HttpRequestError::Status {
                    status,
                    in_session,
                    message: format!("MCP SSE notify '{method}' failed with {status}"),
                });
            }
            Ok(())
        })
        .map_err(HttpRequestError::into_message)
    }

    /// Sends a request with the auth it goes out with. When the server
    /// answers 401, the token is refreshed, once, and the request sent again.
    fn authorized<T>(
        &self,
        mut send: impl FnMut(&AuthAttempt) -> Result<T, HttpRequestError>,
    ) -> Result<T, HttpRequestError> {
        let mut attempt = self.auth.begin()?;
        loop {
            match send(&attempt) {
                Err(error) if error.unauthorized() => {
                    match self.auth.after_unauthorized(&attempt)? {
                        Some(retry) => attempt = retry,
                        None => return Err(error),
                    }
                }
                result => return result,
            }
        }
    }

    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let first = self.post_request(method, params.clone(), timeout);
        self.retry_in_new_session(first, || self.post_request(method, params, timeout))
    }

    fn post_request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, HttpRequestError> {
        self.authorized(|auth| {
            let context = SseRequestContext {
                endpoint: self.endpoint.clone(),
                headers: self.session_headers(auth)?,
                id: self.next_request_id()?,
                method: method.to_string(),
                timeout,
            };
            request_sse_with_client(&self.client, &self.server_name, &context, params.clone())
                .map(|(result, _)| result)
        })
    }

    /// Handles the 404 a server answers once it has ended the session a
    /// request carried: starts a new session and sends the request once more.
    fn retry_in_new_session(
        &self,
        first: Result<Value, HttpRequestError>,
        retry: impl FnOnce() -> Result<Value, HttpRequestError>,
    ) -> Result<Value, String> {
        match first {
            Err(error) if error.ended_session() => {}
            first => return first.map_err(HttpRequestError::into_message),
        }
        self.start_session().map_err(|error| {
            format!(
                "MCP server '{}' ended its session, and starting a new one failed: {}",
                self.server_name,
                error.into_message()
            )
        })?;
        retry().map_err(|error| {
            if error.ended_session() {
                format!(
                    "MCP server '{}' ended its new session too: {}",
                    self.server_name,
                    error.into_message()
                )
            } else {
                error.into_message()
            }
        })
    }

    fn request_with_timeout_or_cancel(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        let first = self.stream_request(method, params.clone(), timeout, handler, should_cancel);
        self.retry_in_new_session(first, || {
            self.stream_request(method, params, timeout, handler, should_cancel)
        })
    }

    fn stream_request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, HttpRequestError> {
        self.authorized(|auth| {
            self.stream_request_with(
                auth,
                method,
                params.clone(),
                timeout,
                handler,
                should_cancel,
            )
        })
    }

    /// Sends one request whose answer may stream, with `auth`.
    fn stream_request_with(
        &self,
        auth: &AuthAttempt,
        method: &str,
        params: Value,
        timeout: Duration,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, HttpRequestError> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string().into());
        }
        let context = SseRequestContext {
            endpoint: self.endpoint.clone(),
            headers: self.session_headers(auth)?,
            id: self.next_request_id()?,
            method: method.to_string(),
            timeout,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let (elicitation_sender, elicitation_receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    HttpRequestError::from(format!(
                        "failed to start MCP SSE request runtime: {error}"
                    ))
                })
                .and_then(|runtime| {
                    runtime.block_on(request_sse_with_async_client(SseAsyncRequest {
                        client: crate::http::client(),
                        context,
                        params,
                        cancel: worker_cancel,
                        elicitation_sender,
                    }))
                });
            let _ = sender.send(result);
        });
        loop {
            match receiver.try_recv() {
                Ok(result) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
                    return result;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
                    return Err("MCP SSE worker stopped before returning".to_string().into());
                }
            }
            if let Ok(envelope) = elicitation_receiver.try_recv() {
                let response =
                    resolve_sse_elicitation(&self.server_name, &envelope.request, handler);
                let _ = envelope.response.send(response);
                continue;
            }
            if should_cancel() {
                cancel.store(true, Ordering::Release);
                while let Ok(envelope) = elicitation_receiver.try_recv() {
                    let _ = envelope.response.send(mcp_jsonrpc_error_response(
                        &envelope.request,
                        -32800,
                        "MCP tool call cancelled".to_string(),
                    ));
                }
                let result = receiver.recv();
                let joined = worker.join();
                if joined.is_err() {
                    return Err("MCP SSE worker panicked during cancellation"
                        .to_string()
                        .into());
                }
                return result
                    .map_err(|_| "MCP SSE worker stopped during cancellation".to_string())?;
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    worker
                        .join()
                        .map_err(|_| "MCP SSE worker panicked before returning".to_string())?;
                    return Err("MCP SSE worker stopped before returning".to_string().into());
                }
            }
        }
    }

    fn next_request_id(&self) -> Result<u64, String> {
        let mut next_id = self
            .next_id
            .lock()
            .map_err(|_| "MCP SSE id lock poisoned".to_string())?;
        let id = *next_id;
        *next_id += 1;
        Ok(id)
    }
}

impl Drop for StreamableHttpTransport {
    /// Ends the server's session, best effort. The DELETE goes out on a
    /// detached thread because a transport may be dropped on an async runtime
    /// thread, where a blocking request panics.
    fn drop(&mut self) {
        let session = match self.session.get_mut() {
            Ok(session) => session.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if session.id.is_none() {
            return;
        }
        let request = self
            .client
            .delete(&self.endpoint)
            .headers(self.request_headers(&session, &self.auth.current()))
            .timeout(HTTP_SESSION_END_TIMEOUT);
        let _ = std::thread::Builder::new()
            .name("mcp-http-session-end".to_string())
            .spawn(move || {
                let _ = request.send();
            });
    }
}

/// `transport = "sse"`: streamable HTTP when the server speaks it, otherwise
/// the legacy HTTP+SSE transport of protocol 2024-11-05. `initialize` picks
/// one the way the MCP spec advises clients that must also reach older
/// servers: it POSTs `initialize`, and only a 400, 404 or 405 answer sends it
/// to legacy SSE. Every later call goes to the transport it picked.
struct SseFallbackTransport {
    config: McpServerConfig,
    /// The auth both transports share, so a token either refreshes serves
    /// the other.
    auth: Arc<RemoteAuth>,
    /// The transport `initialize` picked.
    transport: OnceLock<Box<dyn McpTransport>>,
}

impl SseFallbackTransport {
    fn new(config: &McpServerConfig, auth: Arc<RemoteAuth>) -> Result<Self, String> {
        // Report a missing url or a bad header now, as `http` does.
        remote_url(config)?;
        configured_headers(config)?;
        Ok(Self {
            config: config.clone(),
            auth,
            transport: OnceLock::new(),
        })
    }

    /// Starts a session over streamable HTTP, or over legacy SSE when the
    /// server turns the `initialize` POST away.
    fn start(&self) -> Result<(Box<dyn McpTransport>, Value), String> {
        let http = StreamableHttpTransport::new(&self.config, Arc::clone(&self.auth))?;
        match http.start_session() {
            Ok(result) => Ok((Box::new(http), result)),
            Err(HttpRequestError::Status { status, .. })
                if matches!(status.as_u16(), 400 | 404 | 405) =>
            {
                drop(http);
                // Keep the first failure in view: the server may not speak
                // legacy SSE either. A login the user must make is reported
                // as it is.
                let legacy = LegacySseTransport::connect(&self.config, Arc::clone(&self.auth))
                    .map_err(|error| {
                        if is_auth_required(&error) {
                            error
                        } else {
                            format!("{error}; the streamable HTTP initialize failed with {status}")
                        }
                    })?;
                let result = legacy.initialize()?;
                Ok((Box::new(legacy), result))
            }
            Err(error) => Err(error.into_message()),
        }
    }

    fn transport(&self) -> Result<&dyn McpTransport, String> {
        self.transport
            .get()
            .map(|transport| transport.as_ref())
            .ok_or_else(|| format!("MCP server '{}' is not initialized", self.config.name))
    }
}

impl McpTransport for SseFallbackTransport {
    fn capability_receipt(&self) -> Option<CapabilityReceipt> {
        self.transport.get()?.capability_receipt()
    }

    fn initialize(&self) -> Result<Value, String> {
        if let Some(transport) = self.transport.get() {
            return transport.initialize();
        }
        let (transport, result) = self.start()?;
        // Should two calls race here, the first to finish is kept.
        let _ = self.transport.set(transport);
        Ok(result)
    }

    fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.transport()?.list_tools(cursor)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.transport()?.call_tool(name, arguments)
    }

    fn call_tool_with_elicitation_handler(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
    ) -> Result<Value, String> {
        self.transport()?
            .call_tool_with_elicitation_handler(name, arguments, handler)
    }

    fn call_tool_with_elicitation_handler_or_cancel(
        &self,
        name: &str,
        arguments: Value,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.transport()?
            .call_tool_with_elicitation_handler_or_cancel(name, arguments, handler, should_cancel)
    }

    fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.transport()?.list_resources(cursor)
    }

    fn list_resources_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.transport()?
            .list_resources_or_cancel(cursor, should_cancel)
    }

    fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.transport()?.list_resource_templates(cursor)
    }

    fn list_resource_templates_or_cancel(
        &self,
        cursor: Option<&str>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.transport()?
            .list_resource_templates_or_cancel(cursor, should_cancel)
    }

    fn read_resource(&self, uri: &str) -> Result<Value, String> {
        self.transport()?.read_resource(uri)
    }

    fn read_resource_or_cancel(
        &self,
        uri: &str,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.transport()?
            .read_resource_or_cancel(uri, should_cancel)
    }

    fn list_prompts(&self, cursor: Option<&str>) -> Result<Value, String> {
        self.transport()?.list_prompts(cursor)
    }

    fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.transport()?.get_prompt(name, arguments)
    }

    fn is_closed(&self) -> bool {
        self.transport
            .get()
            .is_some_and(|transport| transport.is_closed())
    }
}

pub(crate) fn resolve_sse_elicitation(
    server_name: &str,
    request: &Value,
    handler: Option<&dyn McpElicitationHandler>,
) -> Value {
    match mcp_elicitation_request_from_json(server_name, request) {
        Ok(request_value) => {
            let decision = match handler {
                Some(handler) => handler.handle_elicitation(request_value),
                None => Ok(McpElicitationResponse::decline()),
            };
            match decision {
                Ok(response) => mcp_elicitation_jsonrpc_response(request, response),
                Err(error) => mcp_jsonrpc_error_response(request, -32000, error),
            }
        }
        Err(error) => mcp_jsonrpc_error_response(request, -32602, error),
    }
}

async fn request_sse_with_async_client(
    request: SseAsyncRequest,
) -> Result<Value, HttpRequestError> {
    let response_future = request
        .client
        .post(&request.context.endpoint)
        .headers(request.context.headers.clone())
        .timeout(request.context.timeout)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": request.context.id,
            "method": request.context.method,
            "params": request.params
        }))
        .send();
    tokio::pin!(response_future);
    let response = loop {
        tokio::select! {
            result = &mut response_future => {
                break result.map_err(|error| {
                    if error.is_timeout() {
                        format!(
                            "MCP SSE request '{}' timed out after {}",
                            request.context.method,
                            format_duration(request.context.timeout)
                        )
                    } else {
                        format!("MCP SSE request '{}' failed: {error}", request.context.method)
                    }
                })?;
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if request.cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string().into());
                }
            }
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Err(HttpRequestError::status(&request.context, status));
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        let text = read_bounded_async_sse_response(response, &request.cancel).await?;
        return Ok(parse_terminal_sse_message(
            &text,
            &request.context.method,
            request.context.id,
        )?);
    }
    Ok(read_sse_stream(
        response,
        &request.cancel,
        request.context,
        request.elicitation_sender,
    )
    .await?)
}

async fn read_sse_stream(
    mut response: reqwest::Response,
    cancel: &AtomicBool,
    context: SseRequestContext,
    elicitation_sender: mpsc::Sender<SseElicitationEnvelope>,
) -> Result<Value, String> {
    let mut decoder = SseDecoder::new();
    let mut total = 0usize;
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result
                .map_err(|error| format!("failed to read MCP SSE response: {error}"))?,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
                continue;
            }
        };
        // A server that streams faster than the wait above never lets it run
        // out: a cancel is looked for after each chunk too.
        if cancel.load(Ordering::Acquire) {
            return Err("MCP tool call cancelled".to_string());
        }
        let Some(chunk) = chunk else {
            return parse_unterminated_sse_event(decoder, &context.method, context.id);
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_SSE_RESPONSE_BYTES {
            return Err(format!(
                "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
            ));
        }
        // An event that cannot be read ends the request, once the events
        // before it are read: `push` gives those out first, and reports the
        // event in the call after. So it is asked again until it has no more.
        let mut bytes: &[u8] = &chunk;
        loop {
            let events = decoder.push(bytes)?;
            bytes = &[];
            if events.is_empty() {
                break;
            }
            for event in events {
                let Some(message) = sse_event_message(&event)? else {
                    continue;
                };
                if is_server_request(&message) {
                    if is_elicitation_create_request(&message) {
                        // The handler is asked on the thread that sent the request.
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        elicitation_sender
                            .send(SseElicitationEnvelope {
                                request: message,
                                response: sender,
                            })
                            .map_err(|_| "MCP SSE elicitation handler stopped".to_string())?;
                        let answer = await_sse_elicitation_response(receiver, cancel).await?;
                        // The server waits for the answer, so one that cannot
                        // reach it ends the request.
                        post_sse_message(
                            &context.endpoint,
                            &context.headers,
                            answer,
                            context.timeout,
                            cancel,
                        )
                        .await?;
                    } else {
                        // Best effort: the request under way may be answered
                        // all the same. A cancel shows at the next read.
                        let _ = post_sse_message(
                            &context.endpoint,
                            &context.headers,
                            server_request_reply(&message),
                            context.timeout,
                            cancel,
                        )
                        .await;
                    }
                } else if is_response_to(&message, context.id) {
                    return parse_terminal_message(message, &context.method, context.id);
                }
            }
        }
    }
}

async fn await_sse_elicitation_response(
    mut receiver: tokio::sync::oneshot::Receiver<Value>,
    cancel: &AtomicBool,
) -> Result<Value, String> {
    loop {
        tokio::select! {
            response = &mut receiver => {
                return response
                    .map_err(|_| "MCP SSE elicitation response was dropped".to_string());
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
            }
        }
    }
}

/// The JSON message an event carries, or `None` when it carries no data.
/// Whitespace around the data is not part of it.
fn sse_event_message(event: &SseEvent) -> Result<Option<Value>, String> {
    let data = event.data.trim();
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(data)
        .map(Some)
        .map_err(|error| format!("invalid MCP SSE event: {error}"))
}

fn parse_terminal_sse_message(text: &str, method: &str, request_id: u64) -> Result<Value, String> {
    let response = parse_sse_or_json_response(text, request_id)
        .map_err(|error| format!("invalid MCP SSE response for '{method}': {error}"))?;
    parse_terminal_message(response, method, request_id)
}

/// The response of a stream that ended without one: what `decoder` has left,
/// an event not followed by a blank line, is read as the response.
fn parse_unterminated_sse_event(
    decoder: SseDecoder,
    method: &str,
    request_id: u64,
) -> Result<Value, String> {
    let message = decoder
        .finish()
        .and_then(|event| event.as_ref().map_or(Ok(None), sse_event_message))
        .map_err(|error| format!("invalid MCP SSE response for '{method}': {error}"))?;
    match message {
        Some(message) => parse_terminal_message(message, method, request_id),
        None => Err(format!("MCP SSE request '{method}' missing result")),
    }
}

pub(crate) fn parse_terminal_message(
    response: Value,
    method: &str,
    request_id: u64,
) -> Result<Value, String> {
    if response.get("id") != Some(&Value::from(request_id)) {
        return Err(format!(
            "MCP SSE request '{method}' returned mismatched response id"
        ));
    }
    if let Some(error) = response.get("error") {
        return Err(format!("MCP SSE request '{method}' failed: {error}"));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("MCP SSE request '{method}' missing result"))
}

async fn post_sse_message(
    endpoint: &str,
    headers: &HeaderMap,
    message: Value,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let response_future = crate::http::client()
        .post(endpoint)
        .headers(headers.clone())
        .timeout(timeout)
        .json(&message)
        .send();
    tokio::pin!(response_future);
    let response = loop {
        tokio::select! {
            result = &mut response_future => {
                break result.map_err(|error| format!("failed to write MCP SSE response: {error}"))?;
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
            }
        }
    };
    if !response.status().is_success() {
        return Err(format!(
            "failed to write MCP SSE response: server returned {}",
            response.status()
        ));
    }
    Ok(())
}

async fn read_bounded_async_sse_response(
    mut response: reqwest::Response,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(MAX_SSE_RESPONSE_BYTES.min(8 * 1024));
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result
                .map_err(|error| format!("failed to read MCP SSE response: {error}"))?,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancel.load(Ordering::Acquire) {
                    return Err("MCP tool call cancelled".to_string());
                }
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_SSE_RESPONSE_BYTES {
            return Err(format!(
                "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("MCP SSE response was not valid UTF-8: {error}"))
}

/// Sends one JSON-RPC request to the server `server_name` and reads its
/// response, which comes back with the response headers. An event stream is
/// read only up to the response, as [`read_sse_response`] says.
fn request_sse_with_client(
    client: &reqwest::blocking::Client,
    server_name: &str,
    context: &SseRequestContext,
    params: Value,
) -> Result<(Value, HeaderMap), HttpRequestError> {
    let method = &context.method;
    let response = client
        .post(&context.endpoint)
        .headers(context.headers.clone())
        .timeout(context.timeout)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": context.id,
            "method": method,
            "params": params
        }))
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                format!(
                    "MCP SSE request '{method}' timed out after {}",
                    format_duration(context.timeout)
                )
            } else {
                format!("MCP SSE request '{method}' failed: {error}")
            }
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(HttpRequestError::status(context, status));
    }
    let headers = response.headers().clone();
    let result = if is_event_stream(&headers) {
        // Replies go to the session the response is in, which the response
        // to `initialize` is the first to name.
        let mut reply_headers = context.headers.clone();
        if let Some(session) = headers
            .get(MCP_SESSION_ID_HEADER)
            .filter(|session| !session.is_empty())
        {
            reply_headers.insert(MCP_SESSION_ID_HEADER, session.clone());
        }
        read_sse_response(client, server_name, context, &reply_headers, response)?
    } else {
        let text = read_bounded_sse_response(response)?;
        parse_terminal_sse_message(&text, method, context.id)?
    };
    Ok((result, headers))
}

/// Whether `headers` say the body is an event stream.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
}

/// Reads the event stream the server `server_name` answers `context`'s
/// request with, up to the response: that ends the read, as the server may
/// keep the stream open after it. Each request the server sends first is
/// answered with a POST that carries `reply_headers`: a question
/// (`elicitation/create`) is declined, as no one is there to ask, and any
/// other request is answered as [`server_request_reply`] says, best effort.
/// Notifications, and responses to other requests, are skipped. At most
/// [`MAX_SSE_RESPONSE_BYTES`] are read.
fn read_sse_response(
    client: &reqwest::blocking::Client,
    server_name: &str,
    context: &SseRequestContext,
    reply_headers: &HeaderMap,
    mut response: reqwest::blocking::Response,
) -> Result<Value, String> {
    let method = &context.method;
    let invalid = |error: String| format!("invalid MCP SSE response for '{method}': {error}");
    let mut decoder = SseDecoder::new();
    let mut total = 0usize;
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        let read = response
            .read(&mut chunk)
            .map_err(|error| format!("failed to read MCP SSE response: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if total > MAX_SSE_RESPONSE_BYTES {
            return Err(format!(
                "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
            ));
        }
        // An event that cannot be read ends the request, once the events
        // before it are read: `push` gives those out first, and reports the
        // event in the call after. So it is asked again until it has no more.
        let mut bytes = &chunk[..read];
        loop {
            let events = decoder.push(bytes).map_err(invalid)?;
            bytes = &[];
            if events.is_empty() {
                break;
            }
            for event in events {
                let Some(message) = sse_event_message(&event).map_err(invalid)? else {
                    continue;
                };
                if is_server_request(&message) {
                    let reply = |answer: &Value| {
                        post_sse_reply(
                            client,
                            &context.endpoint,
                            reply_headers,
                            answer,
                            context.timeout,
                        )
                    };
                    if is_elicitation_create_request(&message) {
                        // The server waits for the answer, so one that cannot
                        // reach it ends the request.
                        reply(&resolve_sse_elicitation(server_name, &message, None))?;
                    } else {
                        // Best effort: the request under way may be answered
                        // all the same.
                        let _ = reply(&server_request_reply(&message));
                    }
                } else if is_response_to(&message, context.id) {
                    return parse_terminal_message(message, method, context.id);
                }
            }
        }
    }
    // The server ended the stream without a response.
    parse_unterminated_sse_event(decoder, method, context.id)
}

/// POSTs `reply`, Orca's answer to a request from the server, to `endpoint`.
fn post_sse_reply(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    headers: &HeaderMap,
    reply: &Value,
    timeout: Duration,
) -> Result<(), String> {
    let response = client
        .post(endpoint)
        .headers(headers.clone())
        .timeout(timeout)
        .json(reply)
        .send()
        .map_err(|error| format!("failed to write MCP SSE response: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to write MCP SSE response: server returned {}",
            response.status()
        ));
    }
    Ok(())
}

fn read_bounded_sse_response(response: reqwest::blocking::Response) -> Result<String, String> {
    let read_limit = MAX_SSE_RESPONSE_BYTES.saturating_add(1) as u64;
    let mut bytes = Vec::with_capacity(MAX_SSE_RESPONSE_BYTES.min(8 * 1024));
    response
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read MCP SSE response: {error}"))?;
    if bytes.len() > MAX_SSE_RESPONSE_BYTES {
        return Err(format!(
            "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("MCP SSE response was not valid UTF-8: {error}"))
}

#[cfg(test)]
fn run_sse_operation<T>(
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to create MCP SSE runtime: {error}"))?
        .block_on(operation)
}

#[cfg(test)]
async fn select_sse_result_or_cancel<T>(
    result: impl std::future::Future<Output = Result<T, String>>,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<T, String> {
    tokio::pin!(result);
    tokio::pin!(cancel);
    tokio::select! {
        biased;
        result = &mut result => result,
        _ = &mut cancel => Err("MCP tool call cancelled".to_string()),
    }
}

/// Reads a JSON body, or an SSE body, where the server's notifications and
/// requests may come before the response. When no event answers
/// `request_id`, the last one is returned for the caller to reject.
fn parse_sse_or_json_response(text: &str, request_id: u64) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Ok(value);
    }

    let mut decoder = SseDecoder::new();
    let events = decoder.push(text.as_bytes())?;
    let mut last = None;
    for event in events.into_iter().chain(decoder.finish()?) {
        if let Some(message) = sse_event_message(&event)? {
            if message.get("id") == Some(&Value::from(request_id))
                && message.get("method").is_none()
            {
                return Ok(message);
            }
            last = Some(message);
        }
    }
    last.ok_or_else(|| "response was neither JSON nor SSE data".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_sse::MCP_SSE_EVENT_STREAM_CLOSED;
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    const STDIO_TEST_STARTUP_TIMEOUT_MS: u64 = 15_000;

    /// Connects to `config` with no stored logins, so that no test reads the
    /// user's.
    fn connect(config: &McpServerConfig) -> Result<Box<dyn McpTransport>, String> {
        connect_with_credentials(config, None)
    }

    #[test]
    fn stdio_json_line_limit_is_enforced_across_small_read_buffers() {
        let mut at_limit = vec![b' '; MAX_STDIO_RESPONSE_LINE_BYTES - 2];
        at_limit.extend_from_slice(b"{}\n");
        let mut reader = BufReader::with_capacity(7, std::io::Cursor::new(at_limit));
        assert_eq!(
            read_json_line(&mut reader).expect("JSON response at byte limit"),
            json!({})
        );

        let mut over_limit = vec![b' '; MAX_STDIO_RESPONSE_LINE_BYTES - 1];
        over_limit.extend_from_slice(b"{}\n");
        let mut reader = BufReader::with_capacity(7, std::io::Cursor::new(over_limit));
        assert!(
            read_json_line(&mut reader)
                .unwrap_err()
                .contains("MCP response exceeded maximum line size")
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_reader_backpressures_unsolicited_response_floods() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("flooding_mcp_server.sh");
        let flood = temp_dir.path().join("responses.jsonl");
        let completed = temp_dir.path().join("flood-completed");
        // Responses to no request of Orca's: the reader queues them even with
        // no request under way, unlike notifications, which it then drops.
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{{\"message\":\"{}\"}}}}\n",
            "x".repeat(1024)
        );
        fs::write(&flood, response.repeat(2048)).expect("write MCP response flood");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"flood","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      cat "$1"
      : > "$2"
      sleep 5
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "flood",
            &server,
            vec![
                flood.to_string_lossy().into_owned(),
                completed.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        transport.initialize().expect("initialize MCP");
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            !completed.exists(),
            "MCP reader drained an unsolicited flood into memory instead of applying backpressure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_oversized_json_line_is_rejected_and_reaps_descendants() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("oversized_mcp_server.sh");
        let response_file = temp_dir.path().join("oversized-response.jsonl");
        let survivor_marker = temp_dir.path().join("oversized-survivor");
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "payload": "x".repeat(MAX_STDIO_RESPONSE_LINE_BYTES) }
        });
        fs::write(
            &response_file,
            format!(
                "{}\n",
                serde_json::to_string(&response).expect("serialize response")
            ),
        )
        .expect("write oversized response");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$2") &
cat "$1"
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "oversized",
            &server,
            vec![
                response_file.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("oversized response must fail");

        assert!(
            error.contains("MCP response exceeded maximum line size"),
            "unexpected oversized response error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_reader_eof_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("closed_stdout_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("reader-eof-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$1") >/dev/null 2>&1 &
exec 1>&-
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "closed-stdout",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("closed MCP stdout must fail");

        assert_eq!(error, "MCP server closed stdout");
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_notification_limit_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("notification_flood_mcp_server.sh");
        let flood = temp_dir.path().join("notifications.jsonl");
        let survivor_marker = temp_dir.path().join("notification-survivor");
        fs::write(
            &flood,
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}
"#
            .repeat(1000),
        )
        .expect("write notification flood");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$2") &
cat "$1"
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "notification-flood",
            &server,
            vec![
                flood.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("notification flood must fail");

        assert!(
            error.contains("exceeded max notification count"),
            "unexpected notification flood error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_malformed_elicitation_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("malformed_elicitation_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("elicitation-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
(sleep 0.4; : > "$1") &
printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create"}\n'
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "malformed-elicitation",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("malformed elicitation must fail");

        assert!(
            error.contains("missing params"),
            "unexpected malformed elicitation error: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_elicitation_write_failure_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir
            .path()
            .join("closed_elicitation_stdin_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("elicitation-write-survivor");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicitation-write","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"authorize","description":"authorizes","inputSchema":{"type":"object"}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      exec 0<&-
      (sleep 2; : > "$1") &
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize"}}\n'
      wait
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "elicitation-write",
            &server,
            vec![survivor_marker.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools(None).expect("list tools");

        let error = transport
            .call_tool("authorize", json!({}))
            .expect_err("closed MCP stdin must reject elicitation response");

        assert!(
            error.contains("failed to write MCP request"),
            "unexpected elicitation write failure: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_request_write_failure_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("closed_stdin_mcp_server.sh");
        let survivor_marker = temp_dir.path().join("write-survivor");
        let ready_marker = temp_dir.path().join("write-ready");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
exec 0<&-
(sleep 0.4; : > "$1") &
: > "$2"
wait
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "closed-stdin",
            &server,
            vec![
                survivor_marker.to_string_lossy().into_owned(),
                ready_marker.to_string_lossy().into_owned(),
            ],
            5_000,
        ))
        .expect("connect stdio MCP");
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready_marker.exists() && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready_marker.exists(),
            "MCP fixture did not close stdin and launch its descendant before the deadline"
        );

        let error = transport
            .initialize()
            .expect_err("closed MCP stdin must fail");

        assert!(
            error.contains("failed to write MCP request"),
            "unexpected write failure: {error}"
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_json_rpc_error_preserves_connection_for_later_requests() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("recoverable_rpc_error_mcp_server.sh");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"recoverable","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"tools unavailable"}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}\n'
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "recoverable",
            &server,
            Vec::new(),
            5_000,
        ))
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");

        let error = transport
            .list_tools(None)
            .expect_err("tools/list RPC error");

        assert!(error.contains("tools unavailable"));
        assert_eq!(
            transport
                .list_resources(None)
                .expect("connection remains usable after JSON-RPC error"),
            json!({ "resources": [] })
        );
    }

    /// A stdio server that, before it answers each `tools/list`, sends the
    /// message in `<dir>/request`, in which `%s` stands for the id of the
    /// `tools/list` it is about to answer, and adds the line it gets back to
    /// `<dir>/replies`. It writes its pid to `<dir>/pid`.
    #[cfg(unix)]
    fn asking_stdio_server(dir: &std::path::Path, request: &str) -> StdioTransport {
        fs::write(dir.join("request"), request).expect("write the server's request");
        let server = dir.join("asking_mcp_server.sh");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
state_dir="$1"
printf '%s\n' "$$" > "$state_dir/pid"
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"asking","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf "$(cat "$state_dir/request")\n" "$id"
      IFS= read -r reply
      printf '%s\n' "$reply" >> "$state_dir/replies"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "asking",
            &server,
            vec![dir.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("start the stdio server");
        transport.initialize().expect("initialize");
        transport
    }

    /// The replies the asking server got, in order.
    #[cfg(unix)]
    fn replies_to_the_server(dir: &std::path::Path) -> Vec<Value> {
        fs::read_to_string(dir.join("replies"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON-RPC reply"))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_server_ping_is_answered() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let transport = asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#,
        );

        let tools = transport
            .list_tools(None)
            .expect("tools/list after the ping");

        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(
            replies_to_the_server(temp_dir.path()),
            [json!({"jsonrpc": "2.0", "id": "p1", "result": {}})]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unknown_server_request_gets_method_not_found() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let transport = asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":"r1","method":"sampling/createMessage","params":{"messages":[],"maxTokens":1}}"#,
        );

        let tools = transport
            .list_tools(None)
            .expect("tools/list after the request");

        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(
            replies_to_the_server(temp_dir.path()),
            [json!({
                "jsonrpc": "2.0",
                "id": "r1",
                "error": {"code": -32601, "message": "Method not found"}
            })]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_server_request_with_our_id_is_not_taken_for_the_response() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        // The ping carries the id of the `tools/list` it comes before.
        let transport = asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":%s,"method":"ping"}"#,
        );

        let tools = transport
            .list_tools(None)
            .expect("tools/list after a ping with its id");

        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(
            replies_to_the_server(temp_dir.path()),
            [json!({"jsonrpc": "2.0", "id": 2, "result": {}})]
        );
        let pid = fs::read_to_string(temp_dir.path().join("pid")).expect("the server's pid");
        assert!(!transport.is_closed(), "the transport was shut down");
        assert!(process_is_alive(pid.trim()), "the server was stopped");
        transport
            .list_tools(None)
            .expect("the server still answers");
    }

    #[cfg(unix)]
    #[test]
    fn a_ping_that_cannot_be_answered_does_not_fail_the_request() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("deaf_mcp_server.sh");
        // It stops reading before it pings, so the answer cannot be written,
        // and then answers `tools/list` all the same.
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"deaf","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      exec 0<&-
      printf '{"jsonrpc":"2.0","id":"p1","method":"ping"}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#,
        );
        let transport =
            StdioTransport::start(&stdio_test_config("deaf", &server, Vec::new(), 5_000))
                .expect("start the stdio server");
        transport.initialize().expect("initialize");

        let tools = transport
            .list_tools(None)
            .expect("tools/list after a ping that could not be answered");

        assert_eq!(tools["tools"][0]["name"], "echo");
    }

    /// A stdio server that, once initialized, sends `messages`, one per line,
    /// and writes the line it gets back to `$REPLY_FILE`. Orca sends it
    /// nothing after `initialize`, so no request of Orca's is under way when
    /// it asks.
    #[cfg(unix)]
    fn idle_asking_stdio_server(
        dir: &std::path::Path,
        messages: &str,
    ) -> (StdioTransport, std::path::PathBuf) {
        let server = dir.join("idle_asking_mcp_server.sh");
        let reply_file = dir.join("reply");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"idle-asking","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      printf '%s\n' "$1"
      IFS= read -r reply
      printf '%s\n' "$reply" > "$REPLY_FILE"
      ;;
  esac
done
"#,
        );
        let mut config =
            stdio_test_config("idle-asking", &server, vec![messages.to_string()], 5_000);
        config.env = HashMap::from([(
            "REPLY_FILE".to_string(),
            reply_file.to_string_lossy().into_owned(),
        )]);
        let transport = StdioTransport::start(&config).expect("start the stdio server");
        transport.initialize().expect("initialize");
        (transport, reply_file)
    }

    /// The reply the idle asking server got, once the whole line is written.
    /// It must come within 2 s.
    #[cfg(unix)]
    fn reply_to_the_idle_server(reply_file: &std::path::Path) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(reply) = fs::read_to_string(reply_file)
                .ok()
                .filter(|reply| reply.ends_with('\n'))
            {
                return serde_json::from_str(&reply).expect("a JSON-RPC reply");
            }
            assert!(
                Instant::now() < deadline,
                "the idle server's request was not answered within 2 s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_idle_stdio_server_gets_its_ping_answered() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let (_transport, reply_file) = idle_asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#,
        );

        assert_eq!(
            reply_to_the_idle_server(&reply_file),
            json!({"jsonrpc": "2.0", "id": "p1", "result": {}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_idle_servers_ping_is_answered_after_a_burst_of_notifications() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        // More log notifications than the reader queues, as a server may send
        // while it is idle, and then a keepalive ping.
        let mut messages: Vec<String> = (0..20)
            .map(|n| {
                format!(
                    r#"{{"jsonrpc":"2.0","method":"notifications/message","params":{{"level":"info","data":"tick {n}"}}}}"#
                )
            })
            .collect();
        messages.push(r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#.to_string());
        let (_transport, reply_file) =
            idle_asking_stdio_server(temp_dir.path(), &messages.join("\n"));

        assert_eq!(
            reply_to_the_idle_server(&reply_file),
            json!({"jsonrpc": "2.0", "id": "p1", "result": {}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unknown_request_from_an_idle_server_gets_method_not_found() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let (_transport, reply_file) = idle_asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":"u1","method":"x/unknown"}"#,
        );

        let reply = reply_to_the_idle_server(&reply_file);

        assert_eq!(reply["id"], "u1");
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[cfg(unix)]
    #[test]
    fn an_elicitation_from_an_idle_server_is_refused() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let (_transport, reply_file) = idle_asking_stdio_server(
            temp_dir.path(),
            r#"{"jsonrpc":"2.0","id":"e1","method":"elicitation/create","params":{"message":"Authorize","requestedSchema":{"type":"object","properties":{}}}}"#,
        );

        let reply = reply_to_the_idle_server(&reply_file);

        assert_eq!(reply["id"], "e1");
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[cfg(unix)]
    fn write_executable_stdio_fixture(path: &std::path::Path, contents: &str) {
        fs::write(path, contents).expect("write MCP fixture");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod MCP fixture");
    }

    #[cfg(unix)]
    fn stdio_test_config(
        name: &str,
        server: &std::path::Path,
        args: Vec<String>,
        timeout_ms: u64,
    ) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: std::iter::once(server.to_string_lossy().into_owned())
                .chain(args)
                .collect(),
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(timeout_ms),
            tool_timeout_ms: Some(timeout_ms),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_uses_configured_tool_timeout() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let survivor_marker = temp_dir.path().join("timeout-survivor");
        let transport = stalling_stdio_transport(&temp_dir, &survivor_marker, 100);

        let started = Instant::now();
        let result = transport.call_tool("wait", Value::Object(Default::default()));

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool call took {:?}",
            started.elapsed()
        );
        assert!(
            result
                .unwrap_err()
                .contains("MCP request 'tools/call' timed out after 100ms")
        );
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_cancel_reaps_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let survivor_marker = temp_dir.path().join("cancel-survivor");
        let transport = stalling_stdio_transport(&temp_dir, &survivor_marker, 5_000);

        let started = Instant::now();
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "wait",
            Value::Object(Default::default()),
            None,
            &|| started.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool cancellation took {:?}",
            started.elapsed()
        );
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert_descendant_did_not_survive(&survivor_marker);
    }

    #[cfg(unix)]
    fn stalling_stdio_transport(
        temp_dir: &tempfile::TempDir,
        survivor_marker: &std::path::Path,
        tool_timeout_ms: u64,
    ) -> Box<dyn McpTransport> {
        let server = temp_dir.path().join("stalling_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      (sleep 0.4; : > "$1") &
      wait
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let transport = connect(&McpServerConfig {
            name: "slow".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![
                server.to_string_lossy().into_owned(),
                survivor_marker.to_string_lossy().into_owned(),
            ],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(tool_timeout_ms),
            ..Default::default()
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools(None).expect("list tools");
        transport
    }

    #[cfg(unix)]
    fn assert_descendant_did_not_survive(survivor_marker: &std::path::Path) {
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !survivor_marker.exists(),
            "MCP descendant continued running after transport termination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_stdio_tool_call_reaps_server_before_returning() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("cancelled_mcp_server.sh");
        let pid_file = temp_dir.path().join("server.pid");
        fs::write(
            &server,
            r#"#!/bin/sh
printf '%s' "$$" > "$PID_FILE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"cancelled","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      IFS= read -r ignored
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let transport = StdioTransport::start(&McpServerConfig {
            name: "cancelled".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: HashMap::from([(
                "PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            )]),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(1000),
            ..Default::default()
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools(None).expect("list tools");
        let started = Instant::now();

        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "wait",
            Value::Object(Default::default()),
            None,
            &|| started.elapsed() >= Duration::from_millis(50),
        );

        let pid = fs::read_to_string(&pid_file).expect("server pid");
        std::thread::sleep(Duration::from_millis(25));
        let server_alive_at_return = process_is_alive(pid.trim());
        drop(transport);

        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            !server_alive_at_return,
            "stdio cancellation returned before the server was waited and reaped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_completed_result_wins_racing_cancellation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("completed_mcp_server.sh");
        let completed_file = temp_dir.path().join("completed");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"completed","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"finish","description":"finishes","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"completed"}],"isError":false}}\n'
      printf completed > "$COMPLETED_FILE"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        let transport = StdioTransport::start(&McpServerConfig {
            name: "completed".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: HashMap::from([(
                "COMPLETED_FILE".to_string(),
                completed_file.to_string_lossy().into_owned(),
            )]),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(1000),
            ..Default::default()
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools(None).expect("list tools");

        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "finish",
            Value::Object(Default::default()),
            None,
            &|| {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !completed_file.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                completed_file.exists()
            },
        );

        assert_eq!(
            result.expect("completed response must win racing cancellation")["content"][0]["text"],
            "completed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_tool_call_routes_elicitation_request_before_final_response() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("elicitation_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicits","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"authorize","description":"needs user input","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize GitHub","url":"https://github.com/login/device","elicitationId":"device-flow"}}\n'
      IFS= read -r response
      case "$response" in
        *'"id":"prompt-1"'*'"action":"accept"'*'"code":"1234"'*)
          printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"authorized"}],"isError":false}}\n'
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"missing elicitation response"}}\n'
          ;;
      esac
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let transport = connect(&McpServerConfig {
            name: "elicits".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![server.to_string_lossy().into_owned()],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(1000),
            ..Default::default()
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools(None).expect("list tools");
        let handler = RecordingElicitationHandler::new(McpElicitationResponse::accept(
            serde_json::json!({"code":"1234"}),
        ));

        let result = transport
            .call_tool_with_elicitation_handler(
                "authorize",
                Value::Object(Default::default()),
                Some(&handler),
            )
            .expect("tool result after elicitation");

        assert_eq!(result["content"][0]["text"], "authorized");
        assert_eq!(
            handler.requests.lock().unwrap().as_slice(),
            &[McpElicitationRequest {
                server_name: "elicits".to_string(),
                id: "prompt-1".to_string(),
                mode: McpElicitationMode::Url,
                message: "Authorize GitHub".to_string(),
                url: Some("https://github.com/login/device".to_string()),
                requested_schema: None,
            }]
        );
    }

    struct RecordingElicitationHandler {
        response: McpElicitationResponse,
        requests: StdMutex<Vec<McpElicitationRequest>>,
    }

    impl RecordingElicitationHandler {
        fn new(response: McpElicitationResponse) -> Self {
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

    #[test]
    fn sse_tool_call_uses_configured_tool_timeout() {
        let server = SlowSseServer::start();
        let transport = connect(&McpServerConfig {
            name: "slow_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(100),
            ..Default::default()
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools(None).expect("list SSE tools");

        let started = Instant::now();
        let result = transport.call_tool("wait", Value::Object(Default::default()));

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "tool call took {:?}",
            started.elapsed()
        );
        assert!(
            result
                .unwrap_err()
                .contains("MCP SSE request 'tools/call' timed out after 100ms")
        );
    }

    #[cfg(unix)]
    #[test]
    fn sse_tool_call_routes_elicitation_request_before_final_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind elicitation SSE fixture");
        listener
            .set_nonblocking(true)
            .expect("set elicitation SSE fixture nonblocking");
        let address = listener
            .local_addr()
            .expect("elicitation SSE fixture address");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut first = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "SSE fixture did not receive tool call"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept SSE elicitation request: {error}"),
                }
            };
            first
                .set_nonblocking(false)
                .expect("set elicitation SSE call blocking");
            first
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("set first SSE read timeout");
            let request = read_http_request(&mut first);
            assert!(request.contains(r#""method":"tools/call""#));
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("write SSE headers");
            first
                .write_all(
                    br#"data: {"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize","url":"https://example.test/device","elicitationId":"device-flow"}}

"#,
                )
                .expect("write elicitation event");
            first.flush().expect("flush elicitation event");

            let mut response = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept SSE elicitation response: {error}"),
                }
            };
            response
                .set_nonblocking(false)
                .expect("set elicitation SSE response blocking");
            let response_request = read_http_request(&mut response);
            assert!(response_request.contains(r#""id":"prompt-1""#));
            assert!(response_request.contains(r#""action":"accept""#));
            write_json_response(&mut response, r#"{"jsonrpc":"2.0","result":{}}"#);
            first
                .write_all(
                    br#"data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"authorized"}],"isError":false}}

"#,
                )
                .expect("write final SSE result");
            first.flush().expect("flush final SSE result");
            true
        });

        let transport = connect(&McpServerConfig {
            name: "elicits-sse".to_string(),
            transport: McpTransportKind::Http,
            command: None,
            args: Vec::new(),
            url: Some(format!("http://{address}")),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
            ..Default::default()
        })
        .expect("connect SSE MCP");
        let handler = RecordingElicitationHandler::new(McpElicitationResponse::accept(
            serde_json::json!({"code":"1234"}),
        ));
        let result = transport
            .call_tool_with_elicitation_handler(
                "authorize",
                Value::Object(Default::default()),
                Some(&handler),
            )
            .expect("tool result after SSE elicitation");
        assert_eq!(result["content"][0]["text"], "authorized");
        assert_eq!(
            handler.requests.lock().unwrap().as_slice(),
            &[McpElicitationRequest {
                server_name: "elicits-sse".to_string(),
                id: "prompt-1".to_string(),
                mode: McpElicitationMode::Url,
                message: "Authorize".to_string(),
                url: Some("https://example.test/device".to_string()),
                requested_schema: None,
            }]
        );
        assert!(server.join().expect("join SSE fixture"));
    }

    #[cfg(unix)]
    #[test]
    fn sse_elicitation_decline_is_observed_over_wire() {
        let (url, server) = start_sse_elicitation_wire_fixture(SseWireElicitationMode::Decline);
        let transport = connect(&McpServerConfig {
            name: "decline-wire".to_string(),
            transport: McpTransportKind::Http,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
            ..Default::default()
        })
        .expect("connect decline SSE MCP");

        let result = transport
            .call_tool_with_elicitation_handler(
                "authorize",
                Value::Object(Default::default()),
                None,
            )
            .expect("declined elicitation should still return terminal tool result");
        assert_eq!(result["content"][0]["text"], "declined");

        let SseWireFixtureObservation::TerminalResponse(observed) =
            server.join().expect("join decline SSE fixture")
        else {
            panic!("decline fixture must observe a terminal response");
        };
        assert_eq!(observed["id"], "prompt-decline");
        assert_eq!(observed["result"]["action"], "decline");
    }

    #[cfg(unix)]
    #[test]
    fn sse_malformed_elicitation_error_is_observed_over_wire() {
        let (url, server) =
            start_sse_elicitation_wire_fixture(SseWireElicitationMode::MalformedParams);
        let transport = connect(&McpServerConfig {
            name: "malformed-wire".to_string(),
            transport: McpTransportKind::Http,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
            ..Default::default()
        })
        .expect("connect malformed SSE MCP");

        let result = transport
            .call_tool_with_elicitation_handler(
                "authorize",
                Value::Object(Default::default()),
                None,
            )
            .expect("malformed elicitation should still return terminal tool result");
        assert_eq!(result["content"][0]["text"], "malformed declined");

        let SseWireFixtureObservation::TerminalResponse(observed) =
            server.join().expect("join malformed SSE fixture")
        else {
            panic!("malformed fixture must observe a terminal response");
        };
        assert_eq!(observed["id"], "prompt-malformed");
        assert_eq!(observed["error"]["code"], -32602);
    }

    #[cfg(unix)]
    #[test]
    fn sse_elicitation_post_cancellation_closes_peer_before_returning() {
        let (url, server) = start_sse_elicitation_wire_fixture(SseWireElicitationMode::StallPost);
        let transport = connect(&McpServerConfig {
            name: "cancel-post-wire".to_string(),
            transport: McpTransportKind::Http,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(2_000),
            ..Default::default()
        })
        .expect("connect cancellation SSE MCP");
        let handler = RecordingElicitationHandler::new(McpElicitationResponse::accept(
            json!({"code":"1234"}),
        ));
        let started = Instant::now();
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "authorize",
            Value::Object(Default::default()),
            Some(&handler),
            &|| started.elapsed() >= Duration::from_millis(100),
        );

        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "elicitation POST cancellation took {:?}",
            started.elapsed()
        );
        let SseWireFixtureObservation::PostPeerClosed(peer_closed) =
            server.join().expect("join stalled POST fixture")
        else {
            panic!("stalled fixture must observe the elicitation POST peer");
        };
        assert!(
            peer_closed,
            "server did not observe the elicitation POST peer close"
        );
    }

    #[test]
    fn sse_elicitation_without_handler_builds_decline_response() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "prompt-decline",
            "method": "elicitation/create",
            "params": {"message": "Authorize"}
        });

        assert_eq!(
            mcp_elicitation_jsonrpc_response(&request, McpElicitationResponse::decline()),
            json!({
                "jsonrpc": "2.0",
                "id": "prompt-decline",
                "result": {"action": "decline"}
            })
        );
    }

    #[test]
    fn malformed_sse_elicitation_builds_typed_json_rpc_error() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "prompt-malformed",
            "method": "elicitation/create"
        });
        let error = mcp_elicitation_request_from_json("server", &request)
            .expect_err("missing params must fail closed");

        assert_eq!(
            mcp_jsonrpc_error_response(&request, -32602, error),
            json!({
                "jsonrpc": "2.0",
                "id": "prompt-malformed",
                "error": {
                    "code": -32602,
                    "message": "MCP elicitation request missing params"
                }
            })
        );

        for params in [Value::Null, json!([]), json!({}), json!({"message": 7})] {
            let request = json!({
                "jsonrpc": "2.0",
                "id": "prompt-malformed-params",
                "method": "elicitation/create",
                "params": params,
            });
            assert_eq!(
                resolve_sse_elicitation("server", &request, None)["error"]["code"],
                -32602
            );
        }
    }

    #[test]
    fn sse_elicitation_resolution_without_handler_declines_and_malformed_fails_closed() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "prompt-decline",
            "method": "elicitation/create",
            "params": {"message": "Authorize"}
        });
        assert_eq!(
            resolve_sse_elicitation("server", &request, None),
            json!({
                "jsonrpc": "2.0",
                "id": "prompt-decline",
                "result": {"action": "decline"}
            })
        );

        let malformed = json!({
            "jsonrpc": "2.0",
            "id": "prompt-malformed",
            "method": "elicitation/create"
        });
        assert_eq!(
            resolve_sse_elicitation("server", &malformed, None)["error"]["code"],
            -32602
        );
    }

    #[test]
    fn sse_terminal_response_rejects_mismatched_request_id() {
        let error = parse_terminal_message(
            json!({"jsonrpc":"2.0","id":99,"result":{"ok":true}}),
            "tools/call",
            1,
        )
        .expect_err("terminal response id must match the request");
        assert!(error.contains("mismatched response id"));
    }

    #[test]
    fn sse_completed_result_wins_racing_cancellation() {
        let expected = json!({"content": [{"type": "text", "text": "completed"}]});

        let result = run_sse_operation(select_sse_result_or_cancel(
            std::future::ready(Ok::<Value, String>(expected.clone())),
            std::future::ready(()),
        ));

        assert_eq!(result, Ok(expected));
    }

    #[test]
    fn sse_cancel_drops_stalled_request_before_returning() {
        let server = CancellableSseServer::start();
        let transport = connect(&McpServerConfig {
            name: "cancellable_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(5000),
            ..Default::default()
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools(None).expect("list SSE tools");
        let started = Instant::now();
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "wait",
            Value::Object(Default::default()),
            None,
            &|| started.elapsed() >= Duration::from_millis(50),
        );
        let cancellation_elapsed = started.elapsed();
        // The fixture accepts serially, so this cannot complete until the stalled socket closes.
        let second = transport
            .call_tool("wait", Value::Object(Default::default()))
            .expect("SSE transport remains usable after cancellation cleanup");
        let reuse_elapsed = started.elapsed();

        assert!(
            cancellation_elapsed < Duration::from_millis(750),
            "tool call took {:?}",
            cancellation_elapsed
        );
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            reuse_elapsed < Duration::from_millis(750),
            "SSE transport was not reusable promptly after cancellation: {reuse_elapsed:?}"
        );
        assert!(
            server.first_request_finished(),
            "cancelled SSE request connection remained active"
        );
        assert_eq!(second["content"][0]["text"], "reconnected");
    }

    #[test]
    fn sse_resource_cancel_drops_stalled_request_before_reuse() {
        let server = CancellableSseServer::start();
        let transport = connect(&McpServerConfig {
            name: "cancellable_resources".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(5000),
            ..Default::default()
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools(None).expect("list SSE tools");
        let started = Instant::now();

        let result = transport
            .list_resources_or_cancel(None, &|| started.elapsed() >= Duration::from_millis(50));
        let cancellation_elapsed = started.elapsed();
        let second = transport
            .list_resources(None)
            .expect("SSE resource transport remains usable after cancellation cleanup");

        assert!(
            cancellation_elapsed < Duration::from_millis(750),
            "resource listing took {cancellation_elapsed:?}"
        );
        assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
        assert!(
            server.first_request_finished(),
            "cancelled SSE resource request remained active"
        );
        assert_eq!(second, json!({"resources": []}));
    }

    struct CancellableSseServer {
        addr: std::net::SocketAddr,
        first_finished: Arc<AtomicBool>,
    }

    impl CancellableSseServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancellable SSE fixture");
            let addr = listener.local_addr().expect("cancellable SSE fixture addr");
            let first_finished = Arc::new(AtomicBool::new(false));
            let finished_for_server = Arc::clone(&first_finished);
            std::thread::spawn(move || {
                let cancellable_calls = AtomicUsize::new(0);
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let request = read_http_request(&mut stream);
                    if request.contains(r#""method":"tools/call""#)
                        || request.contains(r#""method":"resources/list""#)
                    {
                        let call = cancellable_calls.fetch_add(1, Ordering::SeqCst);
                        if call == 0 {
                            stream
                                .set_read_timeout(None)
                                .expect("clear stalled request read timeout");
                            let mut probe = [0u8; 1];
                            while stream.read(&mut probe).is_ok_and(|read| read != 0) {}
                            finished_for_server.store(true, Ordering::SeqCst);
                        } else {
                            if request.contains(r#""method":"resources/list""#) {
                                write_json_response(
                                    &mut stream,
                                    r#"{"jsonrpc":"2.0","id":4,"result":{"resources":[]}}"#,
                                );
                            } else {
                                write_json_response(
                                    &mut stream,
                                    r#"{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}"#,
                                );
                            }
                        }
                    } else if request.contains(r#""method":"tools/list""#) {
                        write_json_response(
                            &mut stream,
                            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}"#,
                        );
                    } else if request.contains(r#""method":"initialize""#) {
                        write_json_response(
                            &mut stream,
                            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"cancellable_sse","version":"1"}}}"#,
                        );
                    } else {
                        write_json_response(&mut stream, r#"{"jsonrpc":"2.0","result":{}}"#);
                    }
                }
            });
            Self {
                addr,
                first_finished,
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn first_request_finished(&self) -> bool {
            self.first_finished.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn sse_handler_cancel_closes_peer_before_returning() {
        let (peer_closed_tx, peer_closed_rx) = mpsc::channel();
        let server = OneShotSseServer::start(move |stream| {
            let _ = read_http_request(stream);
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("set peer-close timeout");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut byte = [0_u8; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) => {
                        let _ = peer_closed_tx.send(true);
                        return;
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => {
                        let _ = peer_closed_tx.send(true);
                        return;
                    }
                }
                if Instant::now() >= deadline {
                    let _ = peer_closed_tx.send(false);
                    return;
                }
            }
        });
        let config = McpServerConfig {
            name: "cancel_peer_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(5000),
            tool_timeout_ms: Some(5000),
            ..Default::default()
        };
        let transport = StreamableHttpTransport::new(&config, no_auth(&config))
            .expect("connect cancellable SSE MCP");
        let started = Instant::now();

        let error = transport
            .call_tool_with_elicitation_handler_or_cancel(
                "wait",
                Value::Object(Default::default()),
                None,
                &|| started.elapsed() >= Duration::from_millis(100),
            )
            .expect_err("SSE tool call should be cancelled");

        assert_eq!(error, "MCP tool call cancelled");
        assert!(
            peer_closed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe cancellation peer close"),
            "cancelled SSE request remained connected after the call returned"
        );
    }

    /// A server that streams progress faster than the reader's 25 ms wait
    /// for a chunk never lets that wait run out, which is where the reader
    /// looked for a cancel. It looks after each chunk too, so a cancel stops
    /// the call mid-stream, well before the stream would end.
    #[test]
    fn a_cancel_stops_a_call_whose_event_stream_keeps_coming() {
        const STREAM_FOR: Duration = Duration::from_secs(5);
        const CANCEL_AFTER: Duration = Duration::from_millis(100);
        let server = OneShotSseServer::start(|stream| {
            let _ = read_http_request(stream);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            let progress = concat!(
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",",
                "\"params\":{\"progressToken\":1,\"progress\":1}}\n\n",
            );
            // An event every 5 ms, until the stream ends or the client hangs up.
            let ends = Instant::now() + STREAM_FOR;
            while Instant::now() < ends
                && stream
                    .write_all(progress.as_bytes())
                    .and_then(|()| stream.flush())
                    .is_ok()
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let config = McpServerConfig {
            name: "streaming".to_string(),
            transport: McpTransportKind::Http,
            url: Some(server.url()),
            startup_timeout_ms: Some(10_000),
            tool_timeout_ms: Some(10_000),
            ..Default::default()
        };
        let transport =
            StreamableHttpTransport::new(&config, no_auth(&config)).expect("an HTTP transport");
        let started = Instant::now();

        let error = transport
            .call_tool_with_elicitation_handler_or_cancel("progress", json!({}), None, &|| {
                started.elapsed() >= CANCEL_AFTER
            })
            .expect_err("the call is cancelled");
        let returned_after = started.elapsed();

        assert_eq!(error, "MCP tool call cancelled");
        assert!(
            returned_after < CANCEL_AFTER + Duration::from_millis(150),
            "the call returned {returned_after:?} after it began, cancelled after \
             {CANCEL_AFTER:?}, with a stream that lasts {STREAM_FOR:?}"
        );
    }

    #[test]
    fn sse_response_body_is_bounded() {
        // A JSON body is read whole; an event stream as its events come.
        for content_type in ["application/json", "text/event-stream"] {
            let server = OneShotSseServer::start(move |stream| {
                let _ = read_http_request(stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n"
                );
                let _ = stream.write_all(&vec![b'x'; MAX_SSE_RESPONSE_BYTES + 1]);
            });

            let error = request_sse_with_client(
                &crate::http::blocking_client(),
                "oversized",
                &SseRequestContext {
                    endpoint: server.url(),
                    headers: HeaderMap::new(),
                    id: 1,
                    method: "tools/list".to_string(),
                    timeout: Duration::from_secs(2),
                },
                json!({}),
            )
            .expect_err("oversized SSE response must be rejected")
            .into_message();

            assert!(
                error.contains("exceeded maximum body size"),
                "unexpected oversized {content_type} response error: {error}"
            );
        }
    }

    /// What one `tools/list` (read by the blocking reader) and one
    /// `tools/call` (read by the async one) come to, each answered by an event
    /// stream that carries `writes`, one write after the other.
    fn both_stream_readers(writes: &[&[u8]]) -> [Result<Value, String>; 2] {
        ["tools/list", "tools/call"].map(|method| {
            let writes = writes.iter().map(|write| write.to_vec()).collect::<Vec<_>>();
            let server = OneShotSseServer::start(move |stream| {
                let _ = read_http_request(stream);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                for write in writes {
                    let _ = stream.write_all(&write);
                    let _ = stream.flush();
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let config = McpServerConfig {
                name: "unreadable".to_string(),
                transport: McpTransportKind::Http,
                url: Some(server.url()),
                startup_timeout_ms: Some(2_000),
                tool_timeout_ms: Some(2_000),
                ..Default::default()
            };
            let transport =
                StreamableHttpTransport::new(&config, no_auth(&config)).expect("an HTTP transport");
            if method == "tools/list" {
                transport.list_tools(None)
            } else {
                transport.call_tool("t", json!({}))
            }
        })
    }

    #[test]
    fn a_response_followed_by_an_unreadable_line_in_the_same_read_still_arrives() {
        let response = br#"data: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let write = [&response[..], b"\n\ndata: \xff\n\n"].concat();

        for result in both_stream_readers(&[&write]) {
            assert_eq!(result, Ok(json!({"ok": true})));
        }
    }

    #[test]
    fn an_unreadable_event_before_the_response_ends_the_request_wherever_the_reads_are_cut() {
        let notification =
            br#"data: {"jsonrpc":"2.0","method":"notifications/message","params":{}}"#;
        let response = br#"data: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let unreadable: &[u8] = b"data: \xff\n\n";
        let notified = [&notification[..], b"\n\n"].concat();
        let answered = [&response[..], b"\n\n"].concat();
        let notified_then_unreadable = [&notified[..], unreadable].concat();
        let everything = [&notified[..], unreadable, &answered[..]].concat();
        let cases: [(&str, Vec<&[u8]>); 3] = [
            ("all in one write", vec![&everything]),
            (
                "the response in a write of its own",
                vec![&notified_then_unreadable, &answered],
            ),
            (
                "each event in a write of its own",
                vec![&notified, unreadable, &answered],
            ),
        ];

        for (cut, writes) in cases {
            for result in both_stream_readers(&writes) {
                let error = result.expect_err(cut);
                assert!(error.to_lowercase().contains("utf-8"), "{cut}: {error}");
            }
        }
    }

    #[test]
    fn a_blocking_sse_read_ends_at_the_matching_response() {
        let (release, released) = mpsc::channel::<()>();
        let server = OneShotSseServer::start(move |stream| {
            let _ = read_http_request(stream);
            let _ = stream.write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"working\"}}\n\n",
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n",
                )
                .as_bytes(),
            );
            let _ = stream.flush();
            // The stream stays open until the test is done.
            let _ = released.recv_timeout(FIXTURE_WAIT);
        });
        let config = McpServerConfig {
            name: "held".to_string(),
            transport: McpTransportKind::Http,
            url: Some(server.url()),
            startup_timeout_ms: Some(10_000),
            ..Default::default()
        };
        let transport =
            StreamableHttpTransport::new(&config, no_auth(&config)).expect("an HTTP transport");
        let started = Instant::now();

        let tools = transport.list_tools(None);

        let elapsed = started.elapsed();
        drop(release);
        assert_eq!(tools, Ok(json!({"tools": []})));
        assert!(
            elapsed < Duration::from_secs(1),
            "the read waited {elapsed:?} for the stream to end"
        );
    }

    #[test]
    fn sse_initialized_notification_uses_startup_timeout() {
        let server = SlowSseServer::start_with_stalling_notification();
        let transport = connect(&McpServerConfig {
            name: "slow_notify_sse".to_string(),
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(server.url()),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(100),
            tool_timeout_ms: Some(100),
            ..Default::default()
        })
        .expect("connect SSE MCP");

        let started = Instant::now();
        let error = transport
            .initialize()
            .expect_err("stalled initialized notification must time out");

        assert!(started.elapsed() < Duration::from_millis(750));
        assert!(
            error.contains("notify 'notifications/initialized' timed out after 100ms"),
            "unexpected notification timeout: {error}"
        );
    }

    struct SlowSseServer {
        addr: std::net::SocketAddr,
    }

    impl SlowSseServer {
        fn start() -> Self {
            Self::start_with_behavior(false)
        }

        fn start_with_stalling_notification() -> Self {
            Self::start_with_behavior(true)
        }

        fn start_with_behavior(stall_notification: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("SSE fixture addr");
            let listener = Arc::new(listener);
            let acceptor = Arc::clone(&listener);
            std::thread::spawn(move || {
                for stream in acceptor.incoming() {
                    match stream {
                        Ok(mut stream) => {
                            handle_sse_fixture_request(&mut stream, stall_notification)
                        }
                        Err(_) => break,
                    }
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    struct OneShotSseServer {
        addr: std::net::SocketAddr,
    }

    impl OneShotSseServer {
        fn start(handler: impl FnOnce(&mut TcpStream) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("SSE fixture addr");
            std::thread::spawn(move || {
                if let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) {
                    handler(&mut stream);
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum SseWireElicitationMode {
        Decline,
        MalformedParams,
        StallPost,
    }

    #[cfg(unix)]
    enum SseWireFixtureObservation {
        TerminalResponse(Value),
        PostPeerClosed(bool),
    }

    #[cfg(unix)]
    fn start_sse_elicitation_wire_fixture(
        mode: SseWireElicitationMode,
    ) -> (String, std::thread::JoinHandle<SseWireFixtureObservation>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind wire SSE fixture");
        listener
            .set_nonblocking(true)
            .expect("set wire SSE fixture nonblocking");
        let address = listener.local_addr().expect("wire SSE fixture address");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut first = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "wire fixture did not receive call"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept wire SSE call: {error}"),
                }
            };
            first
                .set_nonblocking(false)
                .expect("set wire SSE call blocking");
            let request = read_http_request(&mut first);
            assert!(request.contains(r#""method":"tools/call""#));
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("write wire SSE headers");
            let event = match mode {
                SseWireElicitationMode::Decline => json!({
                    "jsonrpc": "2.0",
                    "id": "prompt-decline",
                    "method": "elicitation/create",
                    "params": {"message": "Authorize"},
                }),
                SseWireElicitationMode::MalformedParams => json!({
                    "jsonrpc": "2.0",
                    "id": "prompt-malformed",
                    "method": "elicitation/create",
                    "params": null,
                }),
                SseWireElicitationMode::StallPost => json!({
                    "jsonrpc": "2.0",
                    "id": "prompt-cancel",
                    "method": "elicitation/create",
                    "params": {"message": "Authorize"},
                }),
            };
            first
                .write_all(format!("data: {event}\n\n").as_bytes())
                .expect("write wire elicitation event");
            first.flush().expect("flush wire elicitation event");

            let mut response = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "wire fixture did not receive POST"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept wire SSE POST: {error}"),
                }
            };
            response
                .set_nonblocking(false)
                .expect("set wire SSE POST blocking");
            let response_request = read_http_request(&mut response);
            let body = response_request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let body = serde_json::from_str::<Value>(body).expect("parse wire elicitation body");
            assert_eq!(body["jsonrpc"], "2.0");
            match mode {
                SseWireElicitationMode::Decline => {
                    assert_eq!(body["id"], "prompt-decline");
                    assert_eq!(body["result"]["action"], "decline");
                    write_json_response(
                        &mut response,
                        r#"{"jsonrpc":"2.0","id":"prompt-decline","result":{}}"#,
                    );
                    first
                        .write_all(
                            br#"data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"declined"}],"isError":false}}

"#,
                        )
                        .expect("write decline terminal event");
                    first.flush().expect("flush decline terminal event");
                    SseWireFixtureObservation::TerminalResponse(body)
                }
                SseWireElicitationMode::MalformedParams => {
                    assert_eq!(body["id"], "prompt-malformed");
                    assert_eq!(body["error"]["code"], -32602);
                    write_json_response(
                        &mut response,
                        r#"{"jsonrpc":"2.0","id":"prompt-malformed","result":{}}"#,
                    );
                    first
                        .write_all(
                            br#"data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"malformed declined"}],"isError":false}}

"#,
                        )
                        .expect("write malformed terminal event");
                    first.flush().expect("flush malformed terminal event");
                    SseWireFixtureObservation::TerminalResponse(body)
                }
                SseWireElicitationMode::StallPost => {
                    assert_eq!(body["id"], "prompt-cancel");
                    assert_eq!(body["result"]["action"], "accept");
                    response
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .expect("set stalled POST read timeout");
                    let deadline = Instant::now() + Duration::from_secs(2);
                    let mut byte = [0u8; 1];
                    let peer_closed = loop {
                        match response.read(&mut byte) {
                            Ok(0) => break true,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break true,
                        }
                        if Instant::now() >= deadline {
                            break false;
                        }
                    };
                    SseWireFixtureObservation::PostPeerClosed(peer_closed)
                }
            }
        });
        (format!("http://{address}"), server)
    }

    fn handle_sse_fixture_request(stream: &mut TcpStream, stall_notification: bool) {
        let request = read_http_request(stream);
        if stall_notification && request.contains(r#""method":"notifications/initialized""#) {
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
        if request.contains(r#""method":"tools/call""#) {
            std::thread::sleep(Duration::from_secs(5));
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}"#,
            );
            return;
        }
        if request.contains(r#""method":"tools/list""#) {
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}"#,
            );
            return;
        }
        if request.contains(r#""method":"initialize""#) {
            write_json_response(
                stream,
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow_sse","version":"1"}}}"#,
            );
            return;
        }
        write_json_response(stream, r#"{"jsonrpc":"2.0","result":{}}"#);
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let request = String::from_utf8_lossy(&buffer);
            if let Some(header_end) = request.find("\r\n\r\n") {
                let content_length = request
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= header_end + 4 + content_length {
                    return request.into_owned();
                }
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn write_json_response(stream: &mut TcpStream, body: &str) {
        write_bytes_response(stream, body.as_bytes());
    }

    fn write_bytes_response(stream: &mut TcpStream, body: &[u8]) {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        let _ = stream.write_all(body);
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn http_requests_accept_json_and_event_streams() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior::default());
        for accept in ["*/*", "application/json", "text/event-stream"] {
            let status = crate::http::blocking_client()
                .post(server.url())
                .header("accept", accept)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
                .send()
                .expect("probe the streamable HTTP fixture")
                .status();
            assert_eq!(status, 406, "the fixture must turn away Accept: {accept}");
        }
        let probes = server.requests().len();
        let transport =
            connect(&streamable_http_config("accepting", &server)).expect("connect HTTP MCP");

        transport
            .initialize()
            .expect("initialize with both response types accepted");
        transport.list_tools(None).expect("list tools");
        transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("call a tool");

        let requests = server.requests().split_off(probes);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.rpc_method().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        for request in &requests {
            assert_eq!(
                request.header_values("accept"),
                ["application/json, text/event-stream"],
                "{:?} did not accept both response types",
                request.rpc_method()
            );
        }
    }

    #[test]
    fn http_session_id_is_sent_after_initialize() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1"],
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("sessions", &server)).expect("connect HTTP MCP");

        transport.initialize().expect("initialize");
        transport
            .list_tools(None)
            .expect("list tools in the session");
        transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("call a tool in the session");

        assert_eq!(
            server.session_trail(),
            [
                "initialize",
                "notifications/initialized s1",
                "tools/list s1",
                "tools/call s1"
            ]
        );
    }

    #[test]
    fn http_elicitation_replies_carry_the_session() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            protocol_version: "2025-03-26",
            sessions: vec!["s1"],
            elicit: true,
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("eliciting", &server)).expect("connect HTTP MCP");
        transport.initialize().expect("initialize");
        let handler =
            RecordingElicitationHandler::new(McpElicitationResponse::accept(json!({"ok": true})));

        let result = transport
            .call_tool_with_elicitation_handler("echo", json!({}), Some(&handler))
            .expect("tool result after the elicitation");

        assert_eq!(result["content"][0]["text"], "accept");
        let reply = server
            .requests()
            .into_iter()
            .find(|request| request.body["id"] == "prompt-1")
            .expect("the elicitation reply reached the server");
        assert_eq!(
            reply.header("accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(reply.header("mcp-session-id"), Some("s1"));
        assert_eq!(reply.header("mcp-protocol-version"), Some("2025-03-26"));
    }

    /// Makes each kind of request a streamable HTTP server may answer with
    /// an event stream, to a server that first sends a request of its own
    /// with `method` in each: `initialize` and `tools/list` are read on the
    /// calling thread, and a tool call and a resource listing that can be
    /// cancelled on a thread of their own. Each goes through. Returns the
    /// body of each answer the server got, in order.
    fn answers_to_server_requests(method: &'static str) -> Vec<Value> {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1"],
            asks_first: Some(method),
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("asked", &server)).expect("connect HTTP MCP");

        transport
            .initialize()
            .unwrap_or_else(|error| panic!("initialize after {method}: {error}"));
        transport
            .list_tools(None)
            .unwrap_or_else(|error| panic!("tools/list after {method}: {error}"));
        let called = transport
            .call_tool("echo", json!({"text": "hi"}))
            .unwrap_or_else(|error| panic!("tools/call after {method}: {error}"));
        let resources = transport
            .list_resources_or_cancel(None, &|| false)
            .unwrap_or_else(|error| panic!("resources/list after {method}: {error}"));

        assert_eq!(called["content"][0]["text"], "hi");
        assert_eq!(resources, json!({"resources": []}));
        let answers = answers_to_the_server(&server);
        // Even the answer to the request in the `initialize` response goes
        // to the session that response started.
        for answer in &answers {
            assert_eq!(answer.header("mcp-session-id"), Some("s1"), "{answer:?}");
        }
        answers.into_iter().map(|answer| answer.body).collect()
    }

    /// What the client sent the fixture in answer to its requests.
    fn answers_to_the_server(server: &HttpFixture) -> Vec<HttpExchange> {
        server
            .requests()
            .into_iter()
            .filter(|request| {
                request.body["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("ask-"))
                    && request.rpc_method().is_none()
            })
            .collect()
    }

    /// The answers [`answers_to_server_requests`] expects, `answer` for each
    /// of its four requests, whose ids are 1 to 4.
    fn answers_for_each_request(answer: impl Fn(String) -> Value) -> Vec<Value> {
        (1..=4).map(|id| answer(format!("ask-{id}"))).collect()
    }

    #[test]
    fn http_answers_a_ping_inside_a_response_stream() {
        assert_eq!(
            answers_to_server_requests("ping"),
            answers_for_each_request(|id| json!({"jsonrpc": "2.0", "id": id, "result": {}}))
        );
    }

    #[test]
    fn http_answers_an_unknown_server_request_with_method_not_found() {
        assert_eq!(
            answers_to_server_requests("roots/list"),
            answers_for_each_request(|id| json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }))
        );
    }

    #[test]
    fn http_declines_a_question_when_no_one_can_answer_it() {
        // Only a tool call can have someone to ask, and this one has not.
        assert_eq!(
            answers_to_server_requests("elicitation/create"),
            answers_for_each_request(|id| json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"action": "decline"}
            }))
        );
    }

    #[test]
    fn a_refused_answer_to_a_ping_does_not_fail_the_request() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            asks_first: Some("ping"),
            refuses_replies: true,
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("refusing", &server)).expect("connect HTTP MCP");

        // The server turns away each answer to its ping, and then answers
        // the request all the same.
        transport.initialize().expect("initialize");
        transport.list_tools(None).expect("tools/list");
        let called = transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("the tool call");
        let resources = transport
            .list_resources_or_cancel(None, &|| false)
            .expect("resources/list");

        assert_eq!(called["content"][0]["text"], "hi");
        assert_eq!(resources, json!({"resources": []}));
        assert_eq!(answers_to_the_server(&server).len(), 4);
    }

    #[test]
    fn a_refused_answer_to_a_question_fails_the_request() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            asks_first: Some("elicitation/create"),
            refuses_replies: true,
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("refusing", &server)).expect("connect HTTP MCP");

        // The server waits for the answer, which did not reach it.
        let error = transport
            .initialize()
            .expect_err("the answer to the question was turned away");

        assert_eq!(
            error,
            "failed to write MCP SSE response: server returned 500 Internal Server Error"
        );
    }

    #[test]
    fn http_requests_carry_the_negotiated_protocol_version() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            protocol_version: "2025-03-26",
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("versioned", &server)).expect("connect HTTP MCP");

        let initialized = transport.initialize().expect("initialize");
        transport.list_tools(None).expect("list tools");
        transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("call a tool");

        assert_eq!(initialized["protocolVersion"], "2025-03-26");
        let requests = server.requests();
        assert_eq!(requests[0].rpc_method(), Some("initialize"));
        assert_eq!(requests[0].body["params"]["protocolVersion"], "2025-06-18");
        assert_eq!(requests[0].header("mcp-protocol-version"), None);
        assert_eq!(
            requests[1..]
                .iter()
                .map(|request| (request.rpc_method(), request.header("mcp-protocol-version")))
                .collect::<Vec<_>>(),
            [
                (Some("notifications/initialized"), Some("2025-03-26")),
                (Some("tools/list"), Some("2025-03-26")),
                (Some("tools/call"), Some("2025-03-26")),
            ]
        );
    }

    #[test]
    fn http_notifications_accept_202() {
        for status in [202, 200] {
            let server = StreamableHttpServer::start(StreamableHttpBehavior {
                notification_status: status,
                ..Default::default()
            });
            let transport =
                connect(&streamable_http_config("notified", &server)).expect("connect HTTP MCP");

            transport.initialize().unwrap_or_else(|error| {
                panic!("a notification answered with {status} failed initialize: {error}")
            });

            assert_eq!(server.requests_for("notifications/initialized").len(), 1);
        }

        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            notification_status: 400,
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("refusing", &server)).expect("connect HTTP MCP");
        let error = transport
            .initialize()
            .expect_err("a refused notification must fail initialize");
        assert_eq!(
            error,
            "MCP SSE notify 'notifications/initialized' failed with 400 Bad Request"
        );
    }

    #[test]
    fn an_expired_http_session_is_reinitialized_once() {
        // The server ends session s1 as soon as it starts. Both request paths
        // start session s2 and send the request once more.
        let (server, transport) = expiring_http_session(vec!["s1"]);
        let result = transport
            .call_tool("echo", json!({"text": "again"}))
            .expect("tool call in the new session");
        assert_eq!(result["content"][0]["text"], "again");
        assert_eq!(
            server.session_trail(),
            [
                "initialize",
                "notifications/initialized s1",
                "tools/call s1",
                "initialize",
                "notifications/initialized s2",
                "tools/call s2"
            ]
        );

        assert!(
            server
                .requests_for("initialize")
                .iter()
                .all(|request| request.header("mcp-protocol-version").is_none()),
            "a new session starts without a protocol version header"
        );

        let (server, transport) = expiring_http_session(vec!["s1"]);
        let tools = transport
            .list_tools(None)
            .expect("tool list in the new session");
        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(
            server.session_trail(),
            [
                "initialize",
                "notifications/initialized s1",
                "tools/list s1",
                "initialize",
                "notifications/initialized s2",
                "tools/list s2"
            ]
        );

        // Ended again straight away: one retry, then an error naming the server.
        let (server, transport) = expiring_http_session(vec!["s1", "s2"]);
        let error = transport
            .call_tool("echo", json!({"text": "again"}))
            .expect_err("a second 404 must fail the call");
        assert!(
            error.contains("MCP server 'expiring'") && error.contains("404 Not Found"),
            "unexpected error: {error}"
        );
        assert_eq!(server.requests_for("initialize").len(), 2);
        assert_eq!(server.requests_for("tools/call").len(), 2);
    }

    #[test]
    fn json_and_event_stream_responses_both_work() {
        for event_stream in [false, true] {
            let server = StreamableHttpServer::start(StreamableHttpBehavior {
                sessions: vec!["s1"],
                event_stream,
                ..Default::default()
            });
            let transport =
                connect(&streamable_http_config("formats", &server)).expect("connect HTTP MCP");
            let format = if event_stream { "event stream" } else { "JSON" };

            let initialized = transport
                .initialize()
                .unwrap_or_else(|error| panic!("initialize over {format}: {error}"));
            let tools = transport
                .list_tools(None)
                .unwrap_or_else(|error| panic!("tools/list over {format}: {error}"));
            let called = transport
                .call_tool("echo", json!({"text": "hi"}))
                .unwrap_or_else(|error| panic!("tools/call over {format}: {error}"));
            let resources = transport
                .list_resources_or_cancel(None, &|| false)
                .unwrap_or_else(|error| panic!("resources/list over {format}: {error}"));

            assert_eq!(initialized["serverInfo"]["name"], "fixture", "{format}");
            assert_eq!(tools["tools"][0]["name"], "echo", "{format}");
            assert_eq!(called["content"][0]["text"], "hi", "{format}");
            assert_eq!(resources, json!({"resources": []}), "{format}");
        }
    }

    #[test]
    fn an_unsupported_protocol_version_is_reported() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            protocol_version: "1999-01-01",
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("future", &server)).expect("connect HTTP MCP");

        let error = transport
            .initialize()
            .expect_err("an unsupported protocol version must fail initialize");

        assert_eq!(
            error,
            "MCP server 'future' requires protocol version 1999-01-01, which Orca does not support"
        );
        assert!(
            server.requests_for("notifications/initialized").is_empty(),
            "Orca went on with a protocol version it does not speak"
        );
    }

    #[test]
    fn dropping_an_http_transport_ends_its_session() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1"],
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("closing", &server)).expect("connect HTTP MCP");
        transport.initialize().expect("initialize");

        drop(transport);

        let delete = server
            .wait_for_request(|request| request.method == "DELETE")
            .expect("dropping the transport ends the session");
        assert_eq!(delete.header("mcp-session-id"), Some("s1"));
        assert_eq!(delete.header("mcp-protocol-version"), Some("2025-06-18"));
    }

    #[test]
    fn stdio_launches_take_turns_only_where_pipes_are_not_made_close_on_exec_at_once() {
        let turn = stdio_launch_turn();

        assert_eq!(
            turn.is_some(),
            cfg!(any(target_os = "macos", target_os = "ios")),
            "launches take turns on macOS and iOS only"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdio_declares_the_current_protocol_version() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("recording_mcp_server.sh");
        let recorded = temp_dir.path().join("initialize.json");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
printf '%s\n' "$line" > "$1"
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"recording","version":"1"}}}\n'
while IFS= read -r line; do :; done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "recording",
            &server,
            vec![recorded.to_string_lossy().into_owned()],
            5_000,
        ))
        .expect("connect stdio MCP");

        transport.initialize().expect("initialize MCP");

        let request: Value = serde_json::from_str(
            &fs::read_to_string(&recorded).expect("read the recorded initialize request"),
        )
        .expect("parse the recorded initialize request");
        assert_eq!(request["method"], "initialize");
        assert_eq!(request["params"]["protocolVersion"], "2025-06-18");
    }

    #[cfg(unix)]
    #[test]
    fn an_unsupported_protocol_version_is_reported_by_stdio_servers() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("future_mcp_server.sh");
        write_executable_stdio_fixture(
            &server,
            r#"#!/bin/sh
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1999-01-01","capabilities":{},"serverInfo":{"name":"future","version":"1"}}}\n'
while IFS= read -r line; do :; done
"#,
        );
        let transport = StdioTransport::start(&stdio_test_config(
            "future-stdio",
            &server,
            Vec::new(),
            5_000,
        ))
        .expect("connect stdio MCP");

        let error = transport
            .initialize()
            .expect_err("an unsupported protocol version must fail initialize");

        assert_eq!(
            error,
            "MCP server 'future-stdio' requires protocol version 1999-01-01, which Orca does not support"
        );
    }

    #[test]
    fn http_requests_carry_the_configured_headers() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1"],
            ..Default::default()
        });
        let mut config = streamable_http_config("configured", &server);
        config.headers = HashMap::from([
            ("X-Api-Key".to_string(), "k1".to_string()),
            // The transport sets the protocol headers itself.
            ("Accept".to_string(), "text/plain".to_string()),
            ("Mcp-Session-Id".to_string(), "stale".to_string()),
            ("MCP-Protocol-Version".to_string(), "2024-11-05".to_string()),
        ]);
        let transport = connect(&config).expect("connect HTTP MCP");

        transport.initialize().expect("initialize");
        transport.list_tools(None).expect("list tools");

        assert_eq!(
            server.session_trail(),
            [
                "initialize",
                "notifications/initialized s1",
                "tools/list s1"
            ]
        );
        let requests = server.requests();
        for request in &requests {
            assert_eq!(request.header("x-api-key"), Some("k1"));
            assert_eq!(
                request.header_values("accept"),
                ["application/json, text/event-stream"]
            );
        }
        assert_eq!(requests[0].header("mcp-protocol-version"), None);
        assert_eq!(
            requests[2].header_values("mcp-protocol-version"),
            ["2025-06-18"]
        );
    }

    #[test]
    fn invalid_http_headers_are_reported_without_their_values() {
        let mut config = McpServerConfig {
            name: "misconfigured".to_string(),
            transport: McpTransportKind::Http,
            url: Some("http://127.0.0.1:9".to_string()),
            ..Default::default()
        };
        config.headers = HashMap::from([("Bad Name".to_string(), "secret-1".to_string())]);
        let Err(error) = connect(&config) else {
            panic!("an invalid header name must fail to connect");
        };
        assert_eq!(
            error,
            "MCP server 'misconfigured' has an invalid header name 'Bad Name'"
        );

        config.headers = HashMap::from([("Authorization".to_string(), "secret-2\n".to_string())]);
        let Err(error) = connect(&config) else {
            panic!("an invalid header value must fail to connect");
        };
        assert_eq!(
            error,
            "MCP server 'misconfigured' has an invalid value for header 'Authorization'"
        );
    }

    #[test]
    fn legacy_sse_skips_only_an_unreadable_event() {
        // Each answer comes in the same write as an event that has a line
        // that is not UTF-8. That event is skipped; the answer still arrives.
        let server = LegacySseServer::start(LegacySseBehavior {
            unreadable_before_answers: true,
            ..LegacySseBehavior::default()
        });
        let transport =
            legacy_sse(&legacy_sse_config("legacy", &server)).expect("open the event stream");

        let initialized = transport
            .initialize()
            .expect("initialize over the event stream");
        let tools = transport
            .list_tools(None)
            .expect("tools/list over the event stream");

        assert_eq!(initialized["serverInfo"]["name"], "legacy");
        assert_eq!(tools["tools"][0]["name"], "echo");
    }

    #[test]
    fn legacy_sse_reads_responses_from_the_event_stream() {
        let server = LegacySseServer::start(LegacySseBehavior::default());
        let mut config = legacy_sse_config("legacy", &server);
        config.headers = HashMap::from([("X-Api-Key".to_string(), "k1".to_string())]);
        let transport = legacy_sse(&config).expect("open the event stream");

        let initialized = transport
            .initialize()
            .expect("initialize over the event stream");
        let tools = transport
            .list_tools(None)
            .expect("tools/list over the event stream");
        let called = transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("tools/call over the event stream");

        assert_eq!(initialized["serverInfo"]["name"], "legacy");
        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(called["content"][0]["text"], "hi");
        assert_eq!(
            server.trail(),
            [
                "GET /sse",
                "POST /messages?sessionId=legacy-1 initialize",
                "POST /messages?sessionId=legacy-1 notifications/initialized",
                "POST /messages?sessionId=legacy-1 tools/list",
                "POST /messages?sessionId=legacy-1 tools/call",
            ]
        );
        let requests = server.requests();
        assert_eq!(requests[0].header_values("accept"), ["text/event-stream"]);
        assert_eq!(requests[1].body["params"]["protocolVersion"], "2025-06-18");
        for request in &requests {
            // Legacy SSE has no session or protocol version header.
            assert_eq!(request.header("x-api-key"), Some("k1"), "{}", request.path);
            assert_eq!(request.header("mcp-session-id"), None, "{}", request.path);
            assert_eq!(
                request.header("mcp-protocol-version"),
                None,
                "{}",
                request.path
            );
        }
    }

    #[test]
    fn legacy_sse_rejects_a_cross_origin_endpoint() {
        // Another scheme, another host, and another port.
        for endpoint in [
            "https://127.0.0.1:{port}/messages",
            "http://localhost:{port}/messages",
            "http://127.0.0.1:1/messages",
        ] {
            let server = LegacySseServer::start(LegacySseBehavior {
                endpoint,
                ..Default::default()
            });
            let sent = endpoint.replace("{port}", &server.addr.port().to_string());

            let Err(error) = legacy_sse(&legacy_sse_config("elsewhere", &server)) else {
                panic!("an endpoint on another origin was accepted: {sent}");
            };

            assert_eq!(
                error,
                format!("MCP server 'elsewhere' sent an endpoint on another origin: {sent}")
            );
            assert_eq!(
                server.trail(),
                ["GET /sse"],
                "Orca sent a message to {sent}"
            );
        }
    }

    #[test]
    fn legacy_sse_answers_elicitation_requests_over_post() {
        let server = LegacySseServer::start(LegacySseBehavior {
            elicit: true,
            elicit_unprompted: true,
            ..Default::default()
        });
        let transport =
            legacy_sse(&legacy_sse_config("asking", &server)).expect("open the event stream");

        // A question that comes while no request is waiting is turned down.
        let unprompted = server
            .wait_for_request(|request| request.body["id"] == "unprompted-1")
            .expect("the unprompted question was answered");
        assert_eq!(unprompted.path, "/messages?sessionId=legacy-1");
        assert_eq!(unprompted.body["error"]["code"], -32601);

        transport.initialize().expect("initialize");
        let handler =
            RecordingElicitationHandler::new(McpElicitationResponse::accept(json!({"ok": true})));
        let accepted = transport
            .call_tool_with_elicitation_handler("echo", json!({}), Some(&handler))
            .expect("tool result after the question was answered");
        // Without a handler there is no one to ask, so the question is turned down.
        let refused = transport
            .call_tool("echo", json!({}))
            .expect("tool result after the question was turned down");

        assert_eq!(accepted["content"][0]["text"], "accept");
        assert_eq!(handler.requests.lock().unwrap()[0].message, "Proceed?");
        let reply = server
            .requests()
            .into_iter()
            .find(|request| request.body["id"] == "prompt-1")
            .expect("the reply reached the server");
        assert_eq!(reply.path, "/messages?sessionId=legacy-1");
        assert_eq!(reply.body["result"]["content"], json!({"ok": true}));
        assert_eq!(
            refused["content"][0]["text"],
            "elicitation is not supported"
        );
    }

    /// What the legacy server got in answer to the request with `method` it
    /// sent during a tool call, which went through.
    fn legacy_answer_to_a_server_request(method: &'static str) -> HttpExchange {
        let server = LegacySseServer::start(LegacySseBehavior {
            asks_first: Some(method),
            ..Default::default()
        });
        let transport =
            legacy_sse(&legacy_sse_config("asked", &server)).expect("open the event stream");
        transport.initialize().expect("initialize");

        let called = transport
            .call_tool("echo", json!({"text": "hi"}))
            .unwrap_or_else(|error| panic!("tools/call after {method}: {error}"));

        assert_eq!(called["content"][0]["text"], "hi");
        let answer = server
            .requests()
            .into_iter()
            .find(|request| request.body["id"] == "ask-1")
            .unwrap_or_else(|| panic!("{method} was answered"));
        assert_eq!(answer.path, "/messages?sessionId=legacy-1");
        answer
    }

    #[test]
    fn legacy_sse_answers_a_ping() {
        assert_eq!(
            legacy_answer_to_a_server_request("ping").body,
            json!({"jsonrpc": "2.0", "id": "ask-1", "result": {}})
        );
    }

    #[test]
    fn legacy_sse_answers_an_unknown_server_request_with_method_not_found() {
        assert_eq!(
            legacy_answer_to_a_server_request("roots/list").body,
            json!({
                "jsonrpc": "2.0",
                "id": "ask-1",
                "error": {"code": -32601, "message": "Method not found"}
            })
        );
    }

    #[test]
    fn legacy_sse_times_out_each_request_on_its_own() {
        let server = LegacySseServer::start(LegacySseBehavior {
            unanswered: Some("tools/call"),
            ..Default::default()
        });
        let mut config = legacy_sse_config("slow", &server);
        config.tool_timeout_ms = Some(100);
        let transport = legacy_sse(&config).expect("open the event stream");
        transport.initialize().expect("initialize");

        let started = Instant::now();
        let error = transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect_err("a call that is never answered must time out");
        let elapsed = started.elapsed();

        assert_eq!(error, "MCP SSE request 'tools/call' timed out after 100ms");
        assert!(
            elapsed < Duration::from_secs(2),
            "the call took {elapsed:?}"
        );
        // The stream stays open for the next request, which has its own timeout.
        let tools = transport
            .list_tools(None)
            .expect("tools/list after the timeout");
        assert_eq!(tools["tools"][0]["name"], "echo");
    }

    #[test]
    fn legacy_sse_stops_waiting_when_cancelled() {
        let server = LegacySseServer::start(LegacySseBehavior {
            unanswered: Some("tools/call"),
            ..Default::default()
        });
        let transport =
            legacy_sse(&legacy_sse_config("cancelled", &server)).expect("open the event stream");
        transport.initialize().expect("initialize");

        let started = Instant::now();
        let error = transport
            .call_tool_with_elicitation_handler_or_cancel("echo", json!({}), None, &|| {
                started.elapsed() >= Duration::from_millis(50)
            })
            .expect_err("the call must be cancelled");
        let elapsed = started.elapsed();

        assert_eq!(error, "MCP tool call cancelled");
        assert!(
            elapsed < Duration::from_secs(2),
            "cancelling took {elapsed:?}"
        );
        let tools = transport
            .list_tools(None)
            .expect("tools/list after the cancellation");
        assert_eq!(tools["tools"][0]["name"], "echo");
    }

    #[test]
    fn dropping_a_legacy_sse_transport_stops_its_reader() {
        let server = LegacySseServer::start(LegacySseBehavior::default());
        let transport =
            legacy_sse(&legacy_sse_config("dropped", &server)).expect("open the event stream");
        transport.initialize().expect("initialize");

        // Dropping waits for the reader thread, so a reader that kept reading
        // would hold this thread until the server closed the stream.
        let (dropped, done) = mpsc::channel();
        std::thread::spawn(move || {
            drop(transport);
            let _ = dropped.send(());
        });

        done.recv_timeout(Duration::from_secs(2))
            .expect("dropping the transport stopped its reader");
    }

    #[test]
    fn sse_falls_back_to_legacy_when_post_is_not_allowed() {
        // A server that only speaks legacy SSE answers the POST with 405. The
        // spec has clients fall back on 400 and 404 as well.
        for post_status in [405, 404, 400] {
            let server = LegacySseServer::start(LegacySseBehavior {
                // An absolute endpoint on the same origin works like a relative one.
                endpoint: "http://127.0.0.1:{port}/messages?sessionId={session}",
                post_status,
                ..Default::default()
            });
            let transport =
                connect(&legacy_sse_config("fallback", &server)).expect("connect SSE MCP");

            let initialized = transport
                .initialize()
                .unwrap_or_else(|error| panic!("initialize after a {post_status}: {error}"));
            let tools = transport
                .list_tools(None)
                .unwrap_or_else(|error| panic!("tools/list after a {post_status}: {error}"));
            let called = transport
                .call_tool("echo", json!({"text": "hi"}))
                .unwrap_or_else(|error| panic!("tools/call after a {post_status}: {error}"));

            assert_eq!(initialized["serverInfo"]["name"], "legacy");
            assert_eq!(tools["tools"][0]["name"], "echo");
            assert_eq!(called["content"][0]["text"], "hi");
            assert_eq!(
                server.trail(),
                [
                    "POST /sse initialize",
                    "GET /sse",
                    "POST /messages?sessionId=legacy-1 initialize",
                    "POST /messages?sessionId=legacy-1 notifications/initialized",
                    "POST /messages?sessionId=legacy-1 tools/list",
                    "POST /messages?sessionId=legacy-1 tools/call",
                ],
                "after a {post_status}"
            );
        }
    }

    #[test]
    fn legacy_sse_sends_the_stored_token_on_its_stream_and_posts() {
        let server = LegacySseServer::start(LegacySseBehavior::default());
        let config = legacy_sse_config("legacy", &server);
        let url = config.url.clone().expect("a url");
        let home = tempfile::tempdir().expect("temp dir");
        let credentials = home.path().join("mcp-credentials.json");
        orca_core::config::mcp_credentials::save_mcp_credential(
            &credentials,
            "legacy",
            &orca_core::config::mcp_credentials::McpCredential {
                server_url: url.clone(),
                access_token: "at-1".to_string(),
                refresh_token: None,
                expires_at: None,
                token_endpoint: format!("{}/token", server.url()),
                client_id: "configured-client".to_string(),
                resource: url,
                scope: None,
            },
        )
        .expect("store a login");
        let transport =
            connect_with_credentials(&config, Some(credentials)).expect("connect SSE MCP");

        transport.initialize().expect("initialize over legacy SSE");
        transport
            .list_tools(None)
            .expect("tools/list over legacy SSE");

        assert_eq!(
            server.trail(),
            [
                "POST /sse initialize",
                "GET /sse",
                "POST /messages?sessionId=legacy-1 initialize",
                "POST /messages?sessionId=legacy-1 notifications/initialized",
                "POST /messages?sessionId=legacy-1 tools/list",
            ]
        );
        for request in server.requests() {
            assert_eq!(
                request.header_values("authorization"),
                ["Bearer at-1"],
                "{} {}",
                request.method,
                request.path
            );
        }
    }

    #[test]
    fn sse_falls_back_and_reports_both_failures_when_neither_works() {
        let server = LegacySseServer::start(LegacySseBehavior::default());
        let mut config = legacy_sse_config("nowhere", &server);
        config.url = Some(format!("{}/missing", server.url()));
        let transport = connect(&config).expect("connect SSE MCP");

        let error = transport
            .initialize()
            .expect_err("a url that answers 404 to both transports must fail");

        assert_eq!(
            error,
            "MCP server 'nowhere' could not open its SSE event stream: 404 Not Found; \
             the streamable HTTP initialize failed with 404 Not Found"
        );
        assert_eq!(server.trail(), ["POST /missing initialize", "GET /missing"]);
    }

    #[test]
    fn sse_falls_back_only_when_post_gets_400_404_or_405() {
        let server = LegacySseServer::start(LegacySseBehavior {
            post_status: 500,
            ..Default::default()
        });
        let transport = connect(&legacy_sse_config("failing", &server)).expect("connect SSE MCP");

        let error = transport
            .initialize()
            .expect_err("a 500 must fail initialize");

        assert_eq!(
            error,
            "MCP SSE request 'initialize' failed with 500 Internal Server Error"
        );
        assert_eq!(server.trail(), ["POST /sse initialize"]);
    }

    #[test]
    fn sse_keeps_using_streamable_http_when_it_works() {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1"],
            ..Default::default()
        });
        let mut config = streamable_http_config("modern", &server);
        config.transport = McpTransportKind::Sse;
        let transport = connect(&config).expect("connect SSE MCP");

        // Nothing goes out until `initialize` has picked the transport.
        assert_eq!(
            transport
                .list_tools(None)
                .expect_err("tools/list before initialize"),
            "MCP server 'modern' is not initialized"
        );
        assert!(server.requests().is_empty());

        transport.initialize().expect("initialize");
        let tools = transport.list_tools(None).expect("list tools");
        let called = transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect("call a tool");

        assert_eq!(tools["tools"][0]["name"], "echo");
        assert_eq!(called["content"][0]["text"], "hi");
        assert_eq!(
            server.session_trail(),
            [
                "initialize",
                "notifications/initialized s1",
                "tools/list s1",
                "tools/call s1"
            ]
        );
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.method == "POST"),
            "a server that speaks streamable HTTP was sent a GET"
        );
    }

    #[test]
    fn a_closed_event_stream_fails_pending_requests() {
        let server = LegacySseServer::start(LegacySseBehavior {
            close_stream_on: Some("tools/call"),
            ..Default::default()
        });
        let transport =
            legacy_sse(&legacy_sse_config("closing", &server)).expect("open the event stream");
        transport.initialize().expect("initialize");

        let error = transport
            .call_tool("echo", json!({"text": "hi"}))
            .expect_err("the stream ended before the call was answered");
        assert!(
            error.starts_with(MCP_SSE_EVENT_STREAM_CLOSED),
            "unexpected error: {error}"
        );

        // Later requests fail at once, without sending what no stream can answer.
        let later = transport
            .list_tools(None)
            .expect_err("the event stream is gone");
        assert_eq!(later, error);
        assert!(server.requests_for("tools/list").is_empty());
    }

    #[test]
    fn a_closed_event_stream_is_reopened_for_the_next_call() {
        let server = LegacySseServer::start(LegacySseBehavior {
            close_stream_on: Some("tools/call"),
            ..Default::default()
        });
        let registry = crate::initialize_registry(&[legacy_sse_config("reopening", &server)], None);
        assert!(registry.wait_for_startup(&|| false));
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let echo = registry
            .resolve_tool("mcp__reopening__echo")
            .expect("the echo tool is registered");

        let error = registry
            .call_tool(&echo, json!({"text": "first"}))
            .expect_err("the stream ended under the first call");
        let second = registry
            .call_tool(&echo, json!({"text": "second"}))
            .expect("the next call runs on a new event stream");

        assert!(
            error.starts_with(MCP_SSE_EVENT_STREAM_CLOSED),
            "unexpected error: {error}"
        );
        assert_eq!(second.output, "second");
        let trail = server.trail();
        assert_eq!(
            trail
                .iter()
                .filter(|request| *request == "GET /sse")
                .count(),
            2
        );
        assert_eq!(
            trail.last().map(String::as_str),
            Some("POST /messages?sessionId=legacy-2 tools/call")
        );
    }

    #[test]
    fn remote_transports_ask_for_prompts() {
        let streamable = StreamableHttpServer::start(StreamableHttpBehavior::default());
        let legacy = LegacySseServer::start(LegacySseBehavior::default());
        // The legacy server is reached through the fallback from streamable HTTP.
        for (config, server) in [
            (
                streamable_http_config("streamable", &streamable),
                &streamable,
            ),
            (legacy_sse_config("legacy", &legacy), &legacy),
        ] {
            let transport = connect(&config).expect("connect a remote MCP server");
            transport.initialize().expect("initialize");

            let listed = transport.list_prompts(None).expect("prompts/list");
            let expanded = transport
                .get_prompt("review_pr", json!({"pr": "123"}))
                .expect("prompts/get");

            assert_eq!(listed["prompts"][0]["name"], "review_pr", "{}", config.name);
            assert_eq!(
                expanded["messages"][0]["content"]["text"], "review 123",
                "{}",
                config.name
            );
            assert_eq!(
                server.requests_for("prompts/list").len(),
                1,
                "{}",
                config.name
            );
            let asked = server.requests_for("prompts/get");
            assert_eq!(asked.len(), 1, "{}", config.name);
            assert_eq!(
                asked[0].body["params"],
                json!({"name": "review_pr", "arguments": {"pr": "123"}}),
                "{}",
                config.name
            );
        }
    }

    fn streamable_http_config(name: &str, server: &HttpFixture) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Http,
            url: Some(server.url()),
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(5_000),
            ..Default::default()
        }
    }

    /// A server that ends the listed sessions right after they start, with a
    /// transport already initialized into the first one.
    fn expiring_http_session(
        ended_sessions: Vec<&'static str>,
    ) -> (HttpFixture, Box<dyn McpTransport>) {
        let server = StreamableHttpServer::start(StreamableHttpBehavior {
            sessions: vec!["s1", "s2"],
            ended_sessions,
            ..Default::default()
        });
        let transport =
            connect(&streamable_http_config("expiring", &server)).expect("connect HTTP MCP");
        transport.initialize().expect("initialize");
        (server, transport)
    }

    /// How long an HTTP fixture waits for a request to arrive, or an event
    /// stream for its next event. Only a failing test waits this long.
    const FIXTURE_WAIT: Duration = Duration::from_secs(5);

    /// An HTTP server on a local port. It records every request, so a test
    /// can check exactly what Orca sent, then answers it with the handler it
    /// was started with. Each connection is served on its own thread.
    struct HttpFixture {
        addr: std::net::SocketAddr,
        requests: Arc<StdMutex<Vec<HttpExchange>>>,
        stopped: Arc<AtomicBool>,
    }

    /// A streamable HTTP MCP server. Like a real one it turns away requests
    /// that lack the headers the spec requires.
    struct StreamableHttpServer;

    #[derive(Clone)]
    struct StreamableHttpBehavior {
        /// The version the server answers `initialize` with.
        protocol_version: &'static str,
        /// The session ID each successive `initialize` hands out. With none,
        /// the server does not use sessions.
        sessions: Vec<&'static str>,
        /// Sessions the server ends right after they are initialized: every
        /// later request that carries one gets 404.
        ended_sessions: Vec<&'static str>,
        /// The status the server acknowledges notifications with.
        notification_status: u16,
        /// Answer requests with an SSE stream that carries a log notification
        /// before the response, instead of a JSON body.
        event_stream: bool,
        /// Ask the client a question during `tools/call`, and answer the call
        /// with the action the client replied with.
        elicit: bool,
        /// Answer each request with an event stream that first sends the
        /// client a request of the server's with this method, as
        /// `ask-<the id of the client's request>`, and carries the response
        /// only once the client has answered it.
        asks_first: Option<&'static str>,
        /// Turn away each answer the client sends to a request of the
        /// server's, with 500.
        refuses_replies: bool,
    }

    impl Default for StreamableHttpBehavior {
        fn default() -> Self {
            Self {
                protocol_version: "2025-06-18",
                sessions: Vec::new(),
                ended_sessions: Vec::new(),
                notification_status: 202,
                event_stream: false,
                elicit: false,
                asks_first: None,
                refuses_replies: false,
            }
        }
    }

    /// One HTTP request the fixture received.
    #[derive(Clone, Debug)]
    struct HttpExchange {
        method: String,
        /// The request target: the path and any query.
        path: String,
        /// Header names are lower-cased.
        headers: Vec<(String, String)>,
        /// The JSON body, or null when there is none.
        body: Value,
    }

    impl HttpExchange {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        fn header_values(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect()
        }

        fn rpc_method(&self) -> Option<&str> {
            self.body.get("method").and_then(Value::as_str)
        }
    }

    impl StreamableHttpServer {
        fn start(behavior: StreamableHttpBehavior) -> HttpFixture {
            let initializations = AtomicUsize::new(0);
            HttpFixture::serve(move |stream, request, log| {
                serve_streamable_http(stream, request, &behavior, log, &initializations);
            })
        }
    }

    impl HttpFixture {
        /// Starts serving. Each request is read and recorded, then handed to
        /// `handle` along with every request recorded so far.
        fn serve(
            handle: impl Fn(&mut TcpStream, &HttpExchange, &StdMutex<Vec<HttpExchange>>)
            + Send
            + Sync
            + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
            listener
                .set_nonblocking(true)
                .expect("set HTTP fixture nonblocking");
            let addr = listener.local_addr().expect("HTTP fixture address");
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let log = Arc::clone(&requests);
            let stop = Arc::clone(&stopped);
            let handle = Arc::new(handle);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let log = Arc::clone(&log);
                            let handle = Arc::clone(&handle);
                            std::thread::spawn(move || {
                                let _ = stream.set_nonblocking(false);
                                let Some(request) = read_http_exchange(&mut stream) else {
                                    return;
                                };
                                log.lock().expect("fixture log").push(request.clone());
                                handle(&mut stream, &request, &log);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                addr,
                requests,
                stopped,
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<HttpExchange> {
            self.requests.lock().expect("fixture log").clone()
        }

        fn requests_for(&self, method: &str) -> Vec<HttpExchange> {
            self.requests()
                .into_iter()
                .filter(|request| request.rpc_method() == Some(method))
                .collect()
        }

        /// Each request received, named by its JSON-RPC method and followed
        /// by the session ID it carried.
        fn session_trail(&self) -> Vec<String> {
            self.requests()
                .iter()
                .map(|request| {
                    let method = request.rpc_method().unwrap_or(&request.method);
                    match request.header("mcp-session-id") {
                        Some(session) => format!("{method} {session}"),
                        None => method.to_string(),
                    }
                })
                .collect()
        }

        /// Each request received: its HTTP method and target, then its
        /// JSON-RPC method, if it has one.
        fn trail(&self) -> Vec<String> {
            self.requests()
                .iter()
                .map(|request| match request.rpc_method() {
                    Some(method) => format!("{} {} {method}", request.method, request.path),
                    None => format!("{} {}", request.method, request.path),
                })
                .collect()
        }

        fn wait_for_request(
            &self,
            matches: impl Fn(&HttpExchange) -> bool,
        ) -> Option<HttpExchange> {
            wait_in_log(&self.requests, matches)
        }
    }

    /// Waits up to [`FIXTURE_WAIT`] for a recorded request that `matches`.
    fn wait_in_log(
        log: &StdMutex<Vec<HttpExchange>>,
        matches: impl Fn(&HttpExchange) -> bool,
    ) -> Option<HttpExchange> {
        let deadline = Instant::now() + FIXTURE_WAIT;
        loop {
            if let Some(request) = log
                .lock()
                .expect("fixture log")
                .iter()
                .find(|request| matches(request))
            {
                return Some(request.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    fn serve_streamable_http(
        stream: &mut TcpStream,
        request: &HttpExchange,
        behavior: &StreamableHttpBehavior,
        log: &StdMutex<Vec<HttpExchange>>,
        initializations: &AtomicUsize,
    ) {
        if request.method == "DELETE" {
            return write_http_status(stream, 200);
        }
        let accept = request.header("accept").unwrap_or_default();
        if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
            return write_http_status(stream, 406);
        }
        if request
            .header("mcp-protocol-version")
            .is_some_and(|version| !["2025-06-18", "2025-03-26", "2024-11-05"].contains(&version))
        {
            return write_http_status(stream, 400);
        }
        let session = request.header("mcp-session-id");
        let id = request.body.get("id").cloned();
        if request.rpc_method() == Some("initialize") {
            // A new session starts from an initialize request without an ID.
            if session.is_some() {
                return write_http_status(stream, 400);
            }
            let assigned = behavior
                .sessions
                .get(initializations.fetch_add(1, Ordering::SeqCst))
                .copied();
            let result = json!({
                "protocolVersion": behavior.protocol_version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"}
            });
            return write_rpc_result(stream, behavior, id, result, assigned, log);
        }
        let is_request = id.is_some() && request.rpc_method().is_some();
        if !behavior.sessions.is_empty() {
            let Some(session) = session else {
                return write_http_status(stream, 400);
            };
            if !behavior.sessions.contains(&session)
                || (is_request && behavior.ended_sessions.contains(&session))
            {
                return write_http_status(stream, 404);
            }
        }
        if !is_request {
            let status = match request.rpc_method() {
                Some(_) => behavior.notification_status,
                // The client's reply to a request from the server.
                None if behavior.refuses_replies => 500,
                None => 202,
            };
            return write_http_status(stream, status);
        }
        let result = match request.rpc_method() {
            Some("tools/list") => json!({"tools": [{
                "name": "echo",
                "description": "echoes its text",
                "inputSchema": {"type": "object"}
            }]}),
            Some("tools/call") if behavior.elicit => {
                return elicit_before_answering(stream, id, log);
            }
            Some("tools/call") => json!({
                "content": [{
                    "type": "text",
                    "text": request.body["params"]["arguments"]["text"].clone()
                }],
                "isError": false
            }),
            Some("resources/list") => json!({"resources": []}),
            Some("prompts/list") => fixture_prompt_list(),
            Some("prompts/get") => fixture_prompt(&request.body),
            _ => json!({}),
        };
        write_rpc_result(stream, behavior, id, result, None, log);
    }

    fn write_rpc_result(
        stream: &mut TcpStream,
        behavior: &StreamableHttpBehavior,
        id: Option<Value>,
        result: Value,
        session: Option<&str>,
        log: &StdMutex<Vec<HttpExchange>>,
    ) {
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let session = session
            .map(|session| format!("mcp-session-id: {session}\r\n"))
            .unwrap_or_default();
        if let Some(method) = behavior.asks_first {
            let asked = format!("ask-{}", id.unwrap_or_default());
            let request = server_request(&asked, method);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{session}connection: close\r\n\r\nevent: message\ndata: {request}\n\n"
            );
            let _ = stream.flush();
            if wait_in_log(log, |request| request.body["id"] == asked).is_some() {
                let _ = write!(stream, "event: message\ndata: {response}\n\n");
            }
            return;
        }
        let _ = if behavior.event_stream {
            let log = json!({
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": {"level": "info", "data": "working"}
            });
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{session}connection: close\r\n\r\nevent: message\ndata: {log}\n\nevent: message\ndata: {response}\n\n"
            )
        } else {
            let body = response.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{session}connection: close\r\n\r\n{body}",
                body.len()
            )
        };
    }

    /// Streams an `elicitation/create` request, waits for the client's reply
    /// to reach the server, then answers the call with the replied action.
    fn elicit_before_answering(
        stream: &mut TcpStream,
        id: Option<Value>,
        log: &StdMutex<Vec<HttpExchange>>,
    ) {
        let question = json!({
            "jsonrpc": "2.0",
            "id": "prompt-1",
            "method": "elicitation/create",
            "params": {"message": "Proceed?", "requestedSchema": {"type": "object"}}
        });
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: message\ndata: {question}\n\n"
        );
        let _ = stream.flush();
        let Some(reply) = wait_in_log(log, |request| request.body["id"] == "prompt-1") else {
            return;
        };
        let answer = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{"type": "text", "text": reply.body["result"]["action"]}],
                "isError": false
            }
        });
        let _ = write!(stream, "event: message\ndata: {answer}\n\n");
    }

    fn write_http_status(stream: &mut TcpStream, status: u16) {
        let reason = match status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            406 => "Not Acceptable",
            500 => "Internal Server Error",
            _ => "Unexpected",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
    }

    /// Reads one request without panicking: a fixture thread outlives the
    /// assertions that would report it.
    fn read_http_exchange(stream: &mut TcpStream) -> Option<HttpExchange> {
        stream.set_read_timeout(Some(FIXTURE_WAIT)).ok()?;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next()?.split_whitespace();
            let method = request_line.next()?.to_string();
            let path = request_line.next()?.to_string();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
                .collect::<Vec<_>>();
            let length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = header_end + 4;
            if buffer.len() < body_start + length {
                continue;
            }
            let body = serde_json::from_slice(&buffer[body_start..body_start + length])
                .unwrap_or_default();
            return Some(HttpExchange {
                method,
                path,
                headers,
                body,
            });
        }
    }

    /// The auth of a server with no credentials: none is sent.
    fn no_auth(config: &McpServerConfig) -> Arc<RemoteAuth> {
        Arc::new(RemoteAuth::resolve(config, None).expect("resolve the auth"))
    }

    /// Connects over legacy SSE alone, with no credentials.
    fn legacy_sse(config: &McpServerConfig) -> Result<LegacySseTransport, String> {
        LegacySseTransport::connect(config, no_auth(config))
    }

    fn legacy_sse_config(name: &str, server: &HttpFixture) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Sse,
            url: Some(format!("{}/sse", server.url())),
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(5_000),
            ..Default::default()
        }
    }

    /// A legacy HTTP+SSE MCP server (protocol 2024-11-05). A GET to `/sse`
    /// opens an event stream whose first event names the endpoint to POST
    /// messages to. Each POST gets a bare 202, and a request's response goes
    /// out as a `message` event on the stream, the only place a client can
    /// read it. A POST to `/sse` itself gets `post_status`, as from a server
    /// that predates streamable HTTP.
    struct LegacySseServer;

    #[derive(Clone)]
    struct LegacySseBehavior {
        /// The `endpoint` event's data. `{port}` stands for the server's
        /// port, and `{session}` for the ID of the session the stream opens.
        endpoint: &'static str,
        /// The status a POST to the stream's own url gets.
        post_status: u16,
        /// End the event stream, without an answer, the first time a request
        /// with this method arrives.
        close_stream_on: Option<&'static str>,
        /// Acknowledge requests with this method, but never answer them.
        unanswered: Option<&'static str>,
        /// Ask the client a question during `tools/call`, and answer the call
        /// with the action the client replied, or the error it replied with.
        elicit: bool,
        /// Ask the client a question as soon as the stream opens.
        elicit_unprompted: bool,
        /// During `tools/call`, send the client a request of the server's
        /// with this method, as `ask-1`, and answer the call only once the
        /// client has answered it.
        asks_first: Option<&'static str>,
        /// Send each answer in the same write as an event that has a line
        /// that is not UTF-8, just before it.
        unreadable_before_answers: bool,
    }

    impl Default for LegacySseBehavior {
        fn default() -> Self {
            Self {
                endpoint: "/messages?sessionId={session}",
                post_status: 405,
                close_stream_on: None,
                unanswered: None,
                elicit: false,
                elicit_unprompted: false,
                asks_first: None,
                unreadable_before_answers: false,
            }
        }
    }

    /// What the legacy fixture's connections share.
    #[derive(Default)]
    struct LegacySseState {
        /// Each open event stream, by session ID. It takes the events to
        /// write, or `None` to end the stream.
        streams: StdMutex<HashMap<String, mpsc::Sender<Option<String>>>>,
        /// How many streams have been opened.
        opened: AtomicUsize,
        /// How many questions have been asked during `tools/call`.
        questions: AtomicUsize,
        /// Whether a stream has been ended for `close_stream_on`.
        closed_one: AtomicBool,
    }

    impl LegacySseServer {
        fn start(behavior: LegacySseBehavior) -> HttpFixture {
            let state = LegacySseState::default();
            HttpFixture::serve(move |stream, request, log| {
                serve_legacy_sse(stream, request, &behavior, &state, log);
            })
        }
    }

    fn serve_legacy_sse(
        stream: &mut TcpStream,
        request: &HttpExchange,
        behavior: &LegacySseBehavior,
        state: &LegacySseState,
        log: &StdMutex<Vec<HttpExchange>>,
    ) {
        let (path, query) = request
            .path
            .split_once('?')
            .unwrap_or((request.path.as_str(), ""));
        match (request.method.as_str(), path) {
            ("GET", "/sse") => stream_legacy_events(stream, request, behavior, state),
            ("POST", "/sse") => write_http_status(stream, behavior.post_status),
            ("POST", "/messages") => {
                let events = query.strip_prefix("sessionId=").and_then(|session| {
                    state
                        .streams
                        .lock()
                        .expect("fixture streams")
                        .get(session)
                        .cloned()
                });
                match events {
                    Some(events) => {
                        answer_legacy_message(stream, request, behavior, state, log, &events)
                    }
                    // The session's stream has ended, or never existed.
                    None => write_http_status(stream, 404),
                }
            }
            _ => write_http_status(stream, 404),
        }
    }

    /// Holds a GET open as an event stream: the endpoint first, then each
    /// event the POST handlers send, until one of them ends the stream.
    fn stream_legacy_events(
        stream: &mut TcpStream,
        request: &HttpExchange,
        behavior: &LegacySseBehavior,
        state: &LegacySseState,
    ) {
        if !request
            .header("accept")
            .is_some_and(|accept| accept.contains("text/event-stream"))
        {
            return write_http_status(stream, 406);
        }
        let session = format!("legacy-{}", state.opened.fetch_add(1, Ordering::SeqCst) + 1);
        let port = stream.local_addr().expect("legacy fixture address").port();
        let endpoint = behavior
            .endpoint
            .replace("{port}", &port.to_string())
            .replace("{session}", &session);
        let (sender, events) = mpsc::channel();
        state
            .streams
            .lock()
            .expect("fixture streams")
            .insert(session.clone(), sender);
        let mut opening = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\nevent: endpoint\ndata: {endpoint}\n\n"
        );
        if behavior.elicit_unprompted {
            opening.push_str(&legacy_event(&json!({
                "jsonrpc": "2.0",
                "id": "unprompted-1",
                "method": "elicitation/create",
                "params": {"message": "Anyone there?"}
            })));
        }
        let mut next = Some(opening);
        while let Some(event) = next {
            if stream
                .write_all(&wire_bytes(&event))
                .and_then(|()| stream.flush())
                .is_err()
            {
                break;
            }
            next = events.recv_timeout(FIXTURE_WAIT).ok().flatten();
        }
        state
            .streams
            .lock()
            .expect("fixture streams")
            .remove(&session);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    /// Acknowledges a POSTed message with a bare 202, and answers a request
    /// on the event stream. The answer may reach the client before the 202.
    fn answer_legacy_message(
        stream: &mut TcpStream,
        request: &HttpExchange,
        behavior: &LegacySseBehavior,
        state: &LegacySseState,
        log: &StdMutex<Vec<HttpExchange>>,
        events: &mpsc::Sender<Option<String>>,
    ) {
        let (Some(id), Some(method)) = (request.body.get("id").cloned(), request.rpc_method())
        else {
            // A notification, or the client's reply to a question.
            return write_http_status(stream, 202);
        };
        if behavior.close_stream_on == Some(method)
            && !state.closed_one.swap(true, Ordering::SeqCst)
        {
            let _ = events.send(None);
            return write_http_status(stream, 202);
        }
        if behavior.unanswered == Some(method) {
            return write_http_status(stream, 202);
        }
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "legacy", "version": "1"}
            }),
            "tools/list" => json!({"tools": [{
                "name": "echo",
                "description": "echoes its text",
                "inputSchema": {"type": "object"}
            }]}),
            "tools/call" if behavior.elicit => {
                return ask_before_answering(stream, id, state, log, events);
            }
            "tools/call" if behavior.asks_first.is_some() => {
                return ask_first_then_answer(stream, request, id, behavior, log, events);
            }
            "tools/call" => json!({
                "content": [{
                    "type": "text",
                    "text": request.body["params"]["arguments"]["text"].clone()
                }],
                "isError": false
            }),
            "prompts/list" => fixture_prompt_list(),
            "prompts/get" => fixture_prompt(&request.body),
            _ => json!({}),
        };
        let answer = legacy_event(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        let _ = events.send(Some(if behavior.unreadable_before_answers {
            format!("event: message\ndata: {NOT_UTF8}\n\n{answer}")
        } else {
            answer
        }));
        write_http_status(stream, 202);
    }

    /// Asks the client a question on the event stream, waits for its reply,
    /// then answers the call with the action the client replied, or with the
    /// message of the error it replied with.
    fn ask_before_answering(
        stream: &mut TcpStream,
        id: Value,
        state: &LegacySseState,
        log: &StdMutex<Vec<HttpExchange>>,
        events: &mpsc::Sender<Option<String>>,
    ) {
        let question = format!(
            "prompt-{}",
            state.questions.fetch_add(1, Ordering::SeqCst) + 1
        );
        let _ = events.send(Some(legacy_event(&json!({
            "jsonrpc": "2.0",
            "id": question,
            "method": "elicitation/create",
            "params": {"message": "Proceed?", "requestedSchema": {"type": "object"}}
        }))));
        write_http_status(stream, 202);
        let Some(reply) = wait_in_log(log, |request| request.body["id"] == question) else {
            return;
        };
        let text = reply.body["result"]["action"]
            .as_str()
            .or_else(|| reply.body["error"]["message"].as_str())
            .unwrap_or_default();
        let _ = events.send(Some(legacy_event(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}], "isError": false}
        }))));
    }

    /// Sends the client the request `behavior.asks_first` names on the event
    /// stream, waits for its answer, and then answers the call with its
    /// text.
    fn ask_first_then_answer(
        stream: &mut TcpStream,
        request: &HttpExchange,
        id: Value,
        behavior: &LegacySseBehavior,
        log: &StdMutex<Vec<HttpExchange>>,
        events: &mpsc::Sender<Option<String>>,
    ) {
        let method = behavior.asks_first.unwrap_or("ping");
        let _ = events.send(Some(legacy_event(&server_request("ask-1", method))));
        write_http_status(stream, 202);
        if wait_in_log(log, |request| request.body["id"] == "ask-1").is_none() {
            return;
        }
        let text = request.body["params"]["arguments"]["text"].clone();
        let _ = events.send(Some(legacy_event(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}], "isError": false}
        }))));
    }

    /// A request of the server's with `method`, as `id`. A question
    /// (`elicitation/create`) asks "Proceed?".
    fn server_request(id: &str, method: &str) -> Value {
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if method == "elicitation/create" {
            request["params"] =
                json!({"message": "Proceed?", "requestedSchema": {"type": "object"}});
        }
        request
    }

    fn legacy_event(message: &Value) -> String {
        format!("event: message\ndata: {message}\n\n")
    }

    /// Stands, in an event the fixture sends, for a byte that is not UTF-8.
    const NOT_UTF8: &str = "<not-utf-8>";

    /// What goes on the wire for `event`: its bytes, with a `0xff` where
    /// [`NOT_UTF8`] stands.
    fn wire_bytes(event: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, part) in event.split(NOT_UTF8).enumerate() {
            if index > 0 {
                bytes.push(0xff);
            }
            bytes.extend_from_slice(part.as_bytes());
        }
        bytes
    }

    /// The prompts the HTTP fixtures offer.
    fn fixture_prompt_list() -> Value {
        json!({"prompts": [{
            "name": "review_pr",
            "arguments": [{"name": "pr", "required": true}]
        }]})
    }

    /// The HTTP fixtures' answer to the `prompts/get` request `body`: one
    /// message naming the `pr` argument.
    fn fixture_prompt(body: &Value) -> Value {
        let pr = body["params"]["arguments"]["pr"]
            .as_str()
            .unwrap_or_default();
        json!({"messages": [{
            "role": "user",
            "content": {"type": "text", "text": format!("review {pr}")}
        }]})
    }
}
