//! The stdio transport: an MCP server run as a child process of Orca's, which
//! it speaks JSON-RPC to over the child's stdin and stdout, a message a line.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde_json::{Value, json};

use orca_core::capability::{CapabilityReceipt, EnforcementState};
use orca_core::execution_broker::ExecutionBroker;
use orca_core::mcp_types::McpServerConfig;
use orca_platform::process::ProcessJob;
use orca_platform::shell::resolve_program;

use super::*;

const STDIO_RESPONSE_QUEUE_CAPACITY: usize = 8;

// The largest single MCP response either transport accepts: room for two
// 5 MiB tool images (base64 adds a third) plus text, and far below the 64 MiB
// session record limit a tool result is written under.
pub(crate) const MAX_STDIO_RESPONSE_LINE_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct StdioTransport {
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
    pub(super) fn start(config: &McpServerConfig) -> Result<Self, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_support::*;
    use std::collections::HashMap;
    use std::fs;
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

        // The cancel comes once the server has answered, and once the reader
        // has had time to take the answer from the pipe: what wins is a
        // response the call has observed, not one still on its way.
        let result = transport.call_tool_with_elicitation_handler_or_cancel(
            "finish",
            Value::Object(Default::default()),
            None,
            &|| {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !completed_file.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(200));
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
}
