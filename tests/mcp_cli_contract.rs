//! End-to-end coverage for `orca mcp` and MCP approvals, driven through the
//! real `orca` binary: config round trips, a read-only tool running without
//! approval in suggest mode, a write tool still asking, a permission rule
//! lifting that ask, and a remote streamable HTTP server added by url.
//!
//! Every test gets its own `ORCA_HOME` and working directory. Every `orca`
//! invocation is bounded by [`ORCA_TIMEOUT`]: these tests spawn real MCP
//! server subprocesses and a loopback HTTP fixture, so a genuine hang must
//! fail the one test that caused it rather than the whole nextest run.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Generous bound for one `orca` invocation. Real MCP server subprocesses and
/// a loopback HTTP fixture make timing less predictable than the mock-only
/// contract tests elsewhere, but a genuine hang must still fail the test that
/// caused it, not the whole suite.
const ORCA_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the HTTP fixture waits on a read before giving up on a
/// connection: bounds `read_http_request` so a client that stops mid-request
/// cannot block a fixture thread forever.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The text both fixture MCP servers return from every `tools/call`,
/// regardless of which tool or arguments came in, so a test can prove the
/// call reached the real server without caring which tool ran.
const FIXTURE_TOOL_OUTPUT: &str = "fixture called";

#[test]
fn mcp_commands_round_trip_the_user_config() {
    let fixture = Fixture::new();
    let server = write_fx_mcp_server(fixture.workspace.path());
    let command = server.to_str().expect("fixture path is UTF-8");

    let add = fixture.run(&["mcp", "add", "fx", "--", command]);
    assert_eq!(add.code(), Some(0), "stderr: {}", add.stderr_text());
    assert!(
        add.stdout_text().contains("added MCP server fx"),
        "{}",
        add.stdout_text()
    );

    let list = fixture.run(&["mcp", "list"]);
    assert_eq!(list.code(), Some(0), "stderr: {}", list.stderr_text());
    let expected_line = format!("fx\tstdio\t{command}\t-");
    assert!(
        list.stdout_text().lines().any(|line| line == expected_line),
        "{}",
        list.stdout_text()
    );

    let get = fixture.run(&["mcp", "get", "fx"]);
    assert_eq!(get.code(), Some(0), "stderr: {}", get.stderr_text());
    let get_stdout = get.stdout_text();
    assert!(
        get_stdout.lines().any(|line| line == "name: fx"),
        "{get_stdout}"
    );
    assert!(
        get_stdout
            .lines()
            .any(|line| line == format!("command: {command}")),
        "{get_stdout}"
    );
    assert!(
        get_stdout.lines().any(|line| line == "auth: -"),
        "{get_stdout}"
    );

    let remove = fixture.run(&["mcp", "remove", "fx"]);
    assert_eq!(remove.code(), Some(0), "stderr: {}", remove.stderr_text());
    assert!(
        remove.stdout_text().contains("removed MCP server fx"),
        "{}",
        remove.stdout_text()
    );

    let list_after_remove = fixture.run(&["mcp", "list"]);
    assert_eq!(list_after_remove.code(), Some(0));
    assert!(
        list_after_remove
            .stdout_text()
            .contains("no MCP servers configured"),
        "{}",
        list_after_remove.stdout_text()
    );
}

#[test]
fn a_read_only_mcp_tool_runs_in_suggest_mode_without_asking() {
    let fixture = Fixture::new();
    let server = write_fx_mcp_server(fixture.workspace.path());
    fixture.add_stdio_server("fx", &server);

    let exec = fixture.exec_tool_in_suggest_mode("mcp__fx__lookup");

    assert_eq!(exec.code(), Some(0), "stderr: {}", exec.stderr_text());
    let events = exec.jsonl_events();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "approval.requested"),
        "a read-only MCP tool must not ask for approval in suggest mode: {events:#?}"
    );
    assert!(
        exec.stdout_text().contains(FIXTURE_TOOL_OUTPUT),
        "{}",
        exec.stdout_text()
    );
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn a_write_mcp_tool_still_asks_in_suggest_mode() {
    let fixture = Fixture::new();
    let server = write_fx_mcp_server(fixture.workspace.path());
    fixture.add_stdio_server("fx", &server);

    let exec = fixture.exec_tool_in_suggest_mode("mcp__fx__write_note");

    assert_eq!(exec.code(), Some(3), "stderr: {}", exec.stderr_text());
    let events = exec.jsonl_events();
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "approval.requested"),
        "a write MCP tool must ask for approval in suggest mode: {events:#?}"
    );
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "approval_required"
    );
}

#[test]
fn a_permission_rule_allows_an_mcp_tool_in_suggest_mode() {
    let fixture = Fixture::new();
    let server = write_fx_mcp_server(fixture.workspace.path());
    fixture.add_stdio_server("fx", &server);
    fixture.append_config("\n[[permissions.rules]]\ntool = \"mcp__fx__*\"\ndecision = \"allow\"\n");

    let exec = fixture.exec_tool_in_suggest_mode("mcp__fx__write_note");

    assert_eq!(exec.code(), Some(0), "stderr: {}", exec.stderr_text());
    assert!(
        exec.stdout_text().contains(FIXTURE_TOOL_OUTPUT),
        "{}",
        exec.stdout_text()
    );
    let events = exec.jsonl_events();
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn a_remote_http_server_added_by_url_works() {
    let fixture = Fixture::new();
    let server = HttpMcpFixture::start();
    fixture.add_http_server("web", &server.url());

    let exec = fixture.exec_tool_in_suggest_mode("mcp__web__fetch");

    assert_eq!(exec.code(), Some(0), "stderr: {}", exec.stderr_text());
    let events = exec.jsonl_events();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "approval.requested"),
        "a read-only remote MCP tool must not ask for approval in suggest mode: {events:#?}"
    );
    assert!(
        exec.stdout_text().contains(FIXTURE_TOOL_OUTPUT),
        "{}",
        exec.stdout_text()
    );
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

// ---------------------------------------------------------------------------
// Running the binary, in a temp `ORCA_HOME` and working directory
// ---------------------------------------------------------------------------

/// One finished `orca` invocation: its exit status and fully drained output.
struct FinishedProcess {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl FinishedProcess {
    fn code(&self) -> Option<i32> {
        self.status.code()
    }

    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Parses stdout as one JSON value per line, as `exec --output-format
    /// jsonl` writes it.
    fn jsonl_events(&self) -> Vec<Value> {
        self.stdout_text()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }
}

/// Runs `orca` with `args` under `home`/`workspace`, bounded by
/// [`ORCA_TIMEOUT`]. stdout and stderr are drained on background threads
/// while the main thread polls for exit, so a full pipe buffer can never
/// deadlock the wait; on timeout the child is killed and reaped rather than
/// left to hang the test.
fn run_orca(home: &Path, workspace: &Path, args: &[&str]) -> FinishedProcess {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn orca {args:?}: {error}"));

    let mut stdout_pipe = child.stdout.take().expect("orca stdout");
    let mut stderr_pipe = child.stderr.take().expect("orca stderr");
    let stdout_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + ORCA_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll orca") {
            return FinishedProcess {
                status,
                stdout: stdout_reader.join().expect("join orca stdout reader"),
                stderr: stderr_reader.join().expect("join orca stderr reader"),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("reap timed-out orca");
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            panic!(
                "orca {args:?} did not exit within {ORCA_TIMEOUT:?} (status after kill: {status:?})\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A fresh `ORCA_HOME` and working directory, as every test in this file
/// needs: `orca mcp add` writes to the home's `config.toml`, and `exec` reads
/// it back from there.
struct Fixture {
    home: TempDir,
    workspace: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp ORCA_HOME"),
            workspace: TempDir::new().expect("temp workspace"),
        }
    }

    fn run(&self, args: &[&str]) -> FinishedProcess {
        run_orca(self.home.path(), self.workspace.path(), args)
    }

    /// Appends raw TOML to the home's `config.toml`, alongside whatever
    /// `orca mcp add` already wrote there. Overwriting the file would erase
    /// the `[[mcp_servers]]` entry; appending a distinct top-level table
    /// keeps both.
    fn append_config(&self, extra_toml: &str) {
        let path = self.home.path().join("config.toml");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("open {} for append: {error}", path.display()));
        file.write_all(extra_toml.as_bytes())
            .unwrap_or_else(|error| panic!("append to {}: {error}", path.display()));
    }

    /// Runs `orca mcp add <name> -- <command>`, asserting it succeeds.
    fn add_stdio_server(&self, name: &str, command: &Path) {
        let command = command.to_str().expect("fixture path is UTF-8");
        let add = self.run(&["mcp", "add", name, "--", command]);
        assert_eq!(
            add.code(),
            Some(0),
            "orca mcp add {name}: stderr: {}",
            add.stderr_text()
        );
    }

    /// Runs `orca mcp add <name> --url <url>`, asserting it succeeds. No
    /// `--transport` is passed, so it defaults to streamable HTTP.
    fn add_http_server(&self, name: &str, url: &str) {
        let add = self.run(&["mcp", "add", name, "--url", url]);
        assert_eq!(
            add.code(),
            Some(0),
            "orca mcp add {name}: stderr: {}",
            add.stderr_text()
        );
    }

    /// Runs `exec` for `tool_name` in suggest mode through the hidden mock
    /// provider, which calls exactly the tool name written in the prompt.
    fn exec_tool_in_suggest_mode(&self, tool_name: &str) -> FinishedProcess {
        self.run(&[
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "suggest",
            tool_name,
        ])
    }
}

// ---------------------------------------------------------------------------
// The `fx` stdio fixture: `lookup` (read-only) and `write_note` (not)
// ---------------------------------------------------------------------------

/// Writes the `fx` fixture MCP server: a `/bin/sh` script that speaks
/// line-delimited JSON-RPC over stdio, mirroring
/// `session_server_contract.rs`'s `write_slow_mcp_server`. It advertises two
/// tools, `lookup` (`readOnlyHint: true`) and `write_note` (no hint), and
/// answers every `tools/call` with the literal text "fixture called"
/// (`FIXTURE_TOOL_OUTPUT`), regardless of which tool or arguments came in.
fn write_fx_mcp_server(dir: &Path) -> PathBuf {
    let server = dir.join("fx_mcp_server.sh");
    std::fs::write(
        &server,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fx","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"lookup","description":"looks something up","inputSchema":{"type":"object","properties":{},"required":[]},"annotations":{"readOnlyHint":true}},{"name":"write_note","description":"writes a note","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"fixture called"}],"isError":false}}\n'
      ;;
  esac
done
"#,
    )
    .expect("write fx MCP fixture");
    make_executable(&server);
    server
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fixture");
}

// ---------------------------------------------------------------------------
// The `web` streamable HTTP fixture: one read-only tool, `fetch`
// ---------------------------------------------------------------------------

/// One request the fixture read: the HTTP method, lower-cased header names,
/// and the JSON-RPC body (`Value::Null` for an empty one, as on `DELETE`).
struct HttpRequest {
    method: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn rpc_method(&self) -> Option<&str> {
        self.body.get("method").and_then(Value::as_str)
    }
}

/// A hand-rolled streamable HTTP MCP server, following the client in
/// `crates/orca-mcp/src/transport.rs` (`StreamableHttpTransport`): it checks
/// `Accept`, hands out a session ID on `initialize`, answers a notification
/// with 202, accepts the closing `DELETE`, and answers everything with a
/// plain `application/json` body. It advertises one read-only tool, `fetch`.
///
/// The client may open more than one connection — its blocking client for
/// `initialize`/`list_tools`, then a fresh async client for `tools/call` —
/// and every response here closes its connection (`Connection: close`), so
/// the client always opens a new one instead of racing a reused, already
/// -closed socket.
///
/// Only the *listener* is non-blocking, so the accept loop can poll `stop`
/// without ever blocking inside `accept()`. Each *accepted* stream is put
/// back into blocking mode by `serve_one_mcp_http_request` before it reads
/// anything: on macOS/BSD, a socket `accept()` returns from a non-blocking
/// listener inherits `O_NONBLOCK` (Linux does not do this — POSIX leaves it
/// unspecified). Left non-blocking, `set_read_timeout` never gets a chance
/// to bound anything: the first read on a connection whose request hasn't
/// arrived yet returns `WouldBlock` at once, the handler gives up with no
/// response, and the client sees the connection close before an answer —
/// exactly the "error sending request" `tools/call` reported once every
/// 10–20 runs of the full five-test file, confirmed by temporarily logging
/// the read error kind under a stress run (`kind: WouldBlock` on the very
/// first read, `buffered=0`, os error 35).
struct HttpMcpFixture {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl HttpMcpFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind MCP HTTP fixture");
        listener
            .set_nonblocking(true)
            .expect("set MCP HTTP fixture nonblocking");
        let addr = listener.local_addr().expect("MCP HTTP fixture address");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread::spawn(move || serve_one_mcp_http_request(stream));
                    }
                    // `stop` is the only intended way to end this loop: an
                    // `accept()` failure — `WouldBlock` from the
                    // non-blocking poll, or a spurious OS-level hiccup —
                    // must not tear down the listener. Breaking here would
                    // drop it, refusing every later connection for the rest
                    // of the test.
                    Err(_) => {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        });
        Self { addr, stop }
    }

    /// The MCP endpoint url `orca mcp add --url` should point at.
    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }
}

impl Drop for HttpMcpFixture {
    fn drop(&mut self) {
        // Nextest gives every test its own process, so this thread would be
        // torn down at exit regardless; asking it to stop just lets the
        // accept loop end promptly instead of idling for the rest of the
        // test.
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve_one_mcp_http_request(mut stream: TcpStream) {
    // Un-inherit the listener's non-blocking mode before reading — see
    // `HttpMcpFixture`'s doc comment for why this is load-bearing.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(HTTP_READ_TIMEOUT));
    let Some(request) = read_http_request(&mut stream) else {
        return;
    };

    if request.method == "DELETE" {
        return write_status_only(&mut stream, 200);
    }

    let accept = request.header("accept").unwrap_or_default();
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        return write_status_only(&mut stream, 406);
    }

    let id = request.body.get("id").cloned().unwrap_or(Value::Null);
    match request.rpc_method() {
        Some("initialize") => {
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "web", "version": "1"}
                }
            });
            write_json_response(&mut stream, Some("fixture-session"), &result);
        }
        Some("notifications/initialized") => write_status_only(&mut stream, 202),
        Some("tools/list") => {
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "fetch",
                        "description": "fetches something",
                        "inputSchema": {"type": "object", "properties": {}, "required": []},
                        "annotations": {"readOnlyHint": true}
                    }]
                }
            });
            write_json_response(&mut stream, None, &result);
        }
        Some("tools/call") => {
            let result = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": FIXTURE_TOOL_OUTPUT}],
                    "isError": false
                }
            });
            write_json_response(&mut stream, None, &result);
        }
        _ => {
            let result = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}});
            write_json_response(&mut stream, None, &result);
        }
    }
}

/// Reads one HTTP/1.1 request: headers, then exactly `Content-Length` bytes
/// of body, parsed as JSON (or `Value::Null` for an empty body). Bounded by
/// the stream's read timeout (set by the caller), so a client that stops
/// mid-request drops the connection instead of blocking this thread forever.
fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let method = lines.next()?.split_whitespace().next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let content_length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = header_end + 4;
        if buffer.len() < body_start + content_length {
            continue;
        }
        let body = if content_length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&buffer[body_start..body_start + content_length])
                .unwrap_or(Value::Null)
        };
        return Some(HttpRequest {
            method,
            headers,
            body,
        });
    }
}

fn write_status_only(stream: &mut TcpStream, status: u16) {
    let reason = http_reason_phrase(status);
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
}

/// Writes a `200 OK` JSON-RPC response, with `session_header`'s value as
/// `Mcp-Session-Id` when given. Every response closes its connection, so the
/// client always opens a fresh one for its next request.
fn write_json_response(stream: &mut TcpStream, session_header: Option<&str>, result: &Value) {
    let body = result.to_string();
    let session = session_header
        .map(|id| format!("mcp-session-id: {id}\r\n"))
        .unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{session}connection: close\r\n\r\n{body}",
        body.len()
    );
}

fn http_reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        406 => "Not Acceptable",
        _ => "Unexpected",
    }
}
