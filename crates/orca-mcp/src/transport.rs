use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::{Value, json};

use orca_core::capability::{CapabilityReceipt, EnforcementState};
use orca_core::execution_broker::ExecutionBroker;
use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
use orca_platform::process::ProcessJob;
use orca_platform::shell::resolve_program;

const STDIO_RESPONSE_QUEUE_CAPACITY: usize = 8;
const MAX_STDIO_RESPONSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_SSE_RESPONSE_BYTES: usize = 1024 * 1024;

struct SseElicitationEnvelope {
    request: Value,
    response: tokio::sync::oneshot::Sender<Value>,
}

struct SseRequestContext {
    endpoint: String,
    headers: HashMap<String, String>,
    id: u64,
    method: String,
    timeout: Duration,
}

struct SseAsyncRequest {
    client: reqwest::Client,
    context: SseRequestContext,
    params: Value,
    cancel: Arc<AtomicBool>,
    stream_events: bool,
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
    fn list_tools(&self) -> Result<Value, String>;
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
    fn list_resources(&self) -> Result<Value, String>;
    fn list_resources_or_cancel(&self, should_cancel: &dyn Fn() -> bool) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.list_resources()
    }
    fn list_resource_templates(&self) -> Result<Value, String>;
    fn list_resource_templates_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        self.list_resource_templates()
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
}

pub fn connect(config: &McpServerConfig) -> Result<Box<dyn McpTransport>, String> {
    match config.transport {
        McpTransportKind::Stdio => Ok(Box::new(StdioTransport::start(config)?)),
        McpTransportKind::Sse => Ok(Box::new(SseTransport::new(config)?)),
    }
}

struct StdioTransport {
    server_name: String,
    capability_receipt: CapabilityReceipt,
    state: Mutex<StdioState>,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

struct StdioState {
    child: StdioChild,
    stdin: ChildStdin,
    responses: Option<mpsc::Receiver<Result<Value, String>>>,
    reader_worker: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
}

impl StdioState {
    fn terminate(&mut self) {
        self.responses.take();
        self.child.terminate();
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
        let launched = broker
            .launch_user_trusted(
                child_command,
                format!("mcp:{}", config.name),
                cwd,
                config.capabilities.clone(),
            )
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
        let (response_tx, responses) = mpsc::sync_channel(STDIO_RESPONSE_QUEUE_CAPACITY);
        let reader_worker = std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                match read_json_line(&mut stdout) {
                    Ok(value) => {
                        if response_tx.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            server_name: config.name.clone(),
            capability_receipt: receipt,
            state: Mutex::new(StdioState {
                child,
                stdin,
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
        let result = self.request_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            self.startup_timeout,
            None,
            None,
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request_with_timeout("tools/list", json!({}), self.startup_timeout, None, None)
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

    fn list_resources(&self) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/list",
            json!({}),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_resources_or_cancel(&self, should_cancel: &dyn Fn() -> bool) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/list",
            json!({}),
            self.startup_timeout,
            None,
            Some(should_cancel),
        )
    }

    fn list_resource_templates(&self) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            json!({}),
            self.startup_timeout,
            None,
            None,
        )
    }

    fn list_resource_templates_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout(
            "resources/templates/list",
            json!({}),
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
}

impl StdioTransport {
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
        if let Err(error) = write_json_line(&mut state.stdin, &message) {
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
            if is_elicitation_create_request(&response) {
                if let Err(error) = handle_elicitation_create_request(
                    &self.server_name,
                    &mut state.stdin,
                    &response,
                    elicitation_handler,
                ) {
                    return state.terminal_error(error);
                }
                continue;
            }
            if response.get("id").and_then(Value::as_u64) != Some(id) {
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
        if let Err(error) = write_json_line(
            &mut state.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            }),
        ) {
            return state.terminal_error(error);
        }
        Ok(())
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

fn is_elicitation_create_request(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("elicitation/create")
}

fn handle_elicitation_create_request(
    server_name: &str,
    stdin: &mut ChildStdin,
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
    write_json_line(stdin, &message)
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

fn json_rpc_id_to_string(id: &Value) -> String {
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

fn mcp_jsonrpc_error_response(request: &Value, code: i64, message: String) -> Value {
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

struct SseTransport {
    server_name: String,
    endpoint: String,
    headers: HashMap<String, String>,
    next_id: Mutex<u64>,
    client: reqwest::blocking::Client,
    startup_timeout: Duration,
    tool_timeout: Duration,
}

impl SseTransport {
    fn new(config: &McpServerConfig) -> Result<Self, String> {
        let endpoint = config
            .url
            .clone()
            .ok_or_else(|| format!("MCP SSE server '{}' is missing url", config.name))?;
        Ok(Self {
            server_name: config.name.clone(),
            endpoint,
            headers: config.headers.clone(),
            next_id: Mutex::new(1),
            client: reqwest::blocking::Client::new(),
            startup_timeout: timeout_from_ms(config.startup_timeout_ms),
            tool_timeout: timeout_from_ms(config.tool_timeout_ms),
        })
    }
}

impl McpTransport for SseTransport {
    fn initialize(&self) -> Result<Value, String> {
        let result = self.request_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "orca",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            self.startup_timeout,
        )?;
        self.notify("notifications/initialized", json!({}), self.startup_timeout)?;
        Ok(result)
    }

    fn list_tools(&self) -> Result<Value, String> {
        self.request_with_timeout("tools/list", json!({}), self.startup_timeout)
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

    fn list_resources(&self) -> Result<Value, String> {
        self.request_with_timeout("resources/list", json!({}), self.startup_timeout)
    }

    fn list_resources_or_cancel(&self, should_cancel: &dyn Fn() -> bool) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "resources/list",
            json!({}),
            self.startup_timeout,
            None,
            should_cancel,
        )
    }

    fn list_resource_templates(&self) -> Result<Value, String> {
        self.request_with_timeout("resources/templates/list", json!({}), self.startup_timeout)
    }

    fn list_resource_templates_or_cancel(
        &self,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.request_with_timeout_or_cancel(
            "resources/templates/list",
            json!({}),
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
}

impl SseTransport {
    fn notify(&self, method: &str, params: Value, timeout: Duration) -> Result<(), String> {
        let mut builder = self.client.post(&self.endpoint);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        builder
            .timeout(timeout)
            .json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
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
        Ok(())
    }

    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_request_id()?;
        request_sse_with_client(
            self.client.clone(),
            self.endpoint.clone(),
            self.headers.clone(),
            id,
            method.to_string(),
            params,
            timeout,
        )
    }

    fn request_with_timeout_or_cancel(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        handler: Option<&dyn McpElicitationHandler>,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        if should_cancel() {
            return Err("MCP tool call cancelled".to_string());
        }
        let id = self.next_request_id()?;
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let server_name = self.server_name.clone();
        let method = method.to_string();
        let stream_events = method == "tools/call";
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let (elicitation_sender, elicitation_receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to start MCP SSE request runtime: {error}"))
                .and_then(|runtime| {
                    runtime.block_on(request_sse_with_async_client(SseAsyncRequest {
                        client: reqwest::Client::new(),
                        context: SseRequestContext {
                            endpoint,
                            headers,
                            id,
                            method,
                            timeout,
                        },
                        params,
                        cancel: worker_cancel,
                        stream_events,
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
                    return Err("MCP SSE worker stopped before returning".to_string());
                }
            }
            if let Ok(envelope) = elicitation_receiver.try_recv() {
                let response = resolve_sse_elicitation(&server_name, &envelope.request, handler);
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
                    return Err("MCP SSE worker panicked during cancellation".to_string());
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
                    return Err("MCP SSE worker stopped before returning".to_string());
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

fn resolve_sse_elicitation(
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

async fn request_sse_with_async_client(request: SseAsyncRequest) -> Result<Value, String> {
    let mut builder = request.client.post(&request.context.endpoint);
    for (key, value) in &request.context.headers {
        builder = builder.header(key, value);
    }
    let response_future = builder
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
                    return Err("MCP tool call cancelled".to_string());
                }
            }
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "MCP SSE request '{}' failed with {status}",
            request.context.method
        ));
    }
    if !request.stream_events {
        let text = read_bounded_async_sse_response(response, &request.cancel).await?;
        return parse_terminal_sse_message(&text, &request.context.method, request.context.id);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        let text = read_bounded_async_sse_response(response, &request.cancel).await?;
        return parse_terminal_sse_message(&text, &request.context.method, request.context.id);
    }
    read_sse_stream(
        response,
        &request.cancel,
        request.context,
        request.elicitation_sender,
    )
    .await
}

async fn read_sse_stream(
    mut response: reqwest::Response,
    cancel: &AtomicBool,
    context: SseRequestContext,
    elicitation_sender: mpsc::Sender<SseElicitationEnvelope>,
) -> Result<Value, String> {
    let mut buffer = Vec::new();
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
        let Some(chunk) = chunk else {
            if !buffer.is_empty() {
                let text = String::from_utf8(buffer)
                    .map_err(|error| format!("MCP SSE response was not valid UTF-8: {error}"))?;
                return parse_terminal_sse_message(&text, &context.method, context.id);
            }
            return Err(format!(
                "MCP SSE request '{}' missing result",
                context.method
            ));
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_SSE_RESPONSE_BYTES {
            return Err(format!(
                "MCP SSE response exceeded maximum body size of {MAX_SSE_RESPONSE_BYTES} bytes"
            ));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(end) = sse_event_end(&buffer) {
            let event = buffer.drain(..end).collect::<Vec<_>>();
            let Some(message) = parse_sse_event(&event)? else {
                continue;
            };
            if message.get("method").and_then(Value::as_str) == Some("elicitation/create") {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                elicitation_sender
                    .send(SseElicitationEnvelope {
                        request: message.clone(),
                        response: sender,
                    })
                    .map_err(|_| "MCP SSE elicitation handler stopped".to_string())?;
                let response = await_sse_elicitation_response(receiver, cancel).await?;
                post_sse_message(
                    &context.endpoint,
                    &context.headers,
                    response,
                    context.timeout,
                    cancel,
                )
                .await?;
            } else if message.get("id") == Some(&Value::from(context.id)) {
                return parse_terminal_message(message, &context.method, context.id);
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

fn sse_event_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
        })
}

fn parse_sse_event(event: &[u8]) -> Result<Option<Value>, String> {
    let text = std::str::from_utf8(event).map_err(|error| error.to_string())?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&data)
        .map(Some)
        .map_err(|error| format!("invalid MCP SSE event: {error}"))
}

fn parse_terminal_sse_message(text: &str, method: &str, request_id: u64) -> Result<Value, String> {
    let response = parse_sse_or_json_response(text)
        .map_err(|error| format!("invalid MCP SSE response for '{method}': {error}"))?;
    parse_terminal_message(response, method, request_id)
}

fn parse_terminal_message(response: Value, method: &str, request_id: u64) -> Result<Value, String> {
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
    headers: &HashMap<String, String>,
    message: Value,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut builder = reqwest::Client::new().post(endpoint);
    for (key, value) in headers {
        builder = builder.header(key, value);
    }
    let response_future = builder.timeout(timeout).json(&message).send();
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

fn request_sse_with_client(
    client: reqwest::blocking::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    id: u64,
    method: String,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut builder = client.post(&endpoint);
    for (key, value) in &headers {
        builder = builder.header(key, value);
    }
    let response = builder
        .timeout(timeout)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .send()
        .map_err(|error| {
            if error.is_timeout() {
                format!(
                    "MCP SSE request '{method}' timed out after {}",
                    format_duration(timeout)
                )
            } else {
                format!("MCP SSE request '{method}' failed: {error}")
            }
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("MCP SSE request '{method}' failed with {status}"));
    }
    let text = read_bounded_sse_response(response)?;
    parse_terminal_sse_message(&text, &method, id)
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

fn parse_sse_or_json_response(text: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Ok(value);
    }

    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Err("response was neither JSON nor SSE data".to_string());
    }
    serde_json::from_str(&data).map_err(|error| error.to_string())
}

fn timeout_from_ms(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(30_000).max(1))
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    const STDIO_TEST_STARTUP_TIMEOUT_MS: u64 = 15_000;

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
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"message\":\"{}\"}}}}\n",
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
        transport.list_tools().expect("list tools");

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

        let error = transport.list_tools().expect_err("tools/list RPC error");

        assert!(error.contains("tools unavailable"));
        assert_eq!(
            transport
                .list_resources()
                .expect("connection remains usable after JSON-RPC error"),
            json!({ "resources": [] })
        );
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
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");
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
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");
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
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");

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
        })
        .expect("connect stdio MCP");
        transport.initialize().expect("initialize MCP");
        transport.list_tools().expect("list tools");
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
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools().expect("list SSE tools");

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
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(format!("http://{address}")),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
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
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
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
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(1_000),
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
            transport: McpTransportKind::Sse,
            command: None,
            args: Vec::new(),
            url: Some(url),
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(1_000),
            tool_timeout_ms: Some(2_000),
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
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools().expect("list SSE tools");
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
        })
        .expect("connect SSE MCP");
        transport.initialize().expect("initialize SSE MCP");
        transport.list_tools().expect("list SSE tools");
        let started = Instant::now();

        let result =
            transport.list_resources_or_cancel(&|| started.elapsed() >= Duration::from_millis(50));
        let cancellation_elapsed = started.elapsed();
        let second = transport
            .list_resources()
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
        let transport = SseTransport::new(&McpServerConfig {
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
        })
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

    #[test]
    fn sse_response_body_is_bounded() {
        let server = OneShotSseServer::start(|stream| {
            let _ = read_http_request(stream);
            let body = vec![b'x'; MAX_SSE_RESPONSE_BYTES + 1];
            write_bytes_response(stream, &body);
        });

        let error = request_sse_with_client(
            reqwest::blocking::Client::new(),
            server.url(),
            HashMap::new(),
            1,
            "tools/list".to_string(),
            json!({}),
            Duration::from_secs(2),
        )
        .expect_err("oversized SSE response must be rejected");

        assert!(
            error.contains("exceeded maximum body size"),
            "unexpected oversized response error: {error}"
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
}
