//! The ACP wire baseline for the SDK migration (sub-project 3).
//!
//! Each scenario drives the daemon's own transport with literal JSON-RPC
//! frames, so changing the ACP SDK cannot change the driver. Everything the
//! daemon writes is compared with `src/golden/acp_wire/<scenario>.json` once
//! host-dependent values are masked. Responses and the daemon's requests to
//! the client keep their order; notifications compare as a sorted list,
//! because separate tasks write them. With `ORCA_UPDATE_GOLDEN` set the files
//! are rewritten instead; review the diff like any other change.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::*;

fn client_info() -> Value {
    json!({"name": "wire-baseline", "version": "0.0.0"})
}

fn run(scenario: impl Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, scenario);
}

/// A daemon (shared sessions) or stdio (one connection) ACP endpoint.
struct Endpoint {
    host: RuntimeHost,
    config: RunConfig,
    shared: Option<crate::acp::shared::SharedSessions>,
    connections: Vec<tokio::task::JoinHandle<Result<(), RpcFacadeError>>>,
}

impl Endpoint {
    fn stdio(host: RuntimeHost, cwd: &Path) -> Self {
        Self {
            host,
            config: test_config(cwd.to_path_buf()),
            shared: None,
            connections: Vec::new(),
        }
    }

    fn daemon(host: RuntimeHost, cwd: &Path) -> Self {
        Self {
            shared: Some(crate::acp::shared::SharedSessions::default()),
            ..Self::stdio(host, cwd)
        }
    }

    fn connect(&mut self) -> WirePeer {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let surface = self.host.surface_handle();
        let config = self.config.clone();
        self.connections.push(match &self.shared {
            Some(shared) => tokio::task::spawn_local(run_shared_connection(
                surface,
                config,
                server_read,
                server_write,
                shared.clone(),
            )),
            None => {
                tokio::task::spawn_local(run_connection(surface, config, server_read, server_write))
            }
        });
        WirePeer {
            write: client_write,
            read: BufReader::new(client_read),
            frames: Vec::new(),
            methods: HashMap::new(),
        }
    }

    async fn shut_down(self) {
        for connection in self.connections {
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
        }
        self.host.shutdown().unwrap();
    }
}

/// One client connection; records every frame the daemon writes to it.
struct WirePeer {
    write: WriteHalf<DuplexStream>,
    read: BufReader<ReadHalf<DuplexStream>>,
    frames: Vec<Value>,
    /// Method of each request this peer sent, by its id.
    methods: HashMap<String, String>,
}

impl WirePeer {
    async fn send(&mut self, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        self.write.write_all(&bytes).await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn start(&mut self, id: Value, method: &str, params: Value) {
        self.methods.insert(id.to_string(), method.to_string());
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
    }

    /// Sends a request and reads until its response.
    async fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.start(id.clone(), method, params).await;
        self.response(&id).await
    }

    async fn response(&mut self, id: &Value) -> Value {
        self.until(|frame| frame.get("method").is_none() && frame["id"] == *id)
            .await
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        loop {
            let frame = self
                .next()
                .await
                .expect("daemon closed the connection early");
            if done(&frame) {
                return frame;
            }
        }
    }

    /// Like `until`, but a frame that already arrived counts.
    async fn seen(&mut self, done: impl Fn(&Value) -> bool) {
        if !self.frames.iter().any(&done) {
            self.until(done).await;
        }
    }

    async fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(TEST_TIMEOUT, self.read.read_line(&mut line))
            .await
            .expect("ACP frame timeout")
            .unwrap();
        if read == 0 {
            return None;
        }
        let frame: Value = serde_json::from_str(&line).unwrap();
        self.frames.push(frame.clone());
        Some(frame)
    }

    /// Hangs up, then records whatever the daemon still writes.
    async fn hang_up(&mut self) {
        self.write.shutdown().await.unwrap();
        while self.next().await.is_some() {}
    }

    /// Answers the daemon's requests to the client until `prompt_id`'s response.
    async fn serve_client(&mut self, prompt_id: &Value, permission: &str) -> Value {
        loop {
            let frame = self
                .until(|frame| {
                    frame.get("id").is_some()
                        && (frame.get("method").is_some() || frame["id"] == *prompt_id)
                })
                .await;
            let Some(method) = frame["method"].as_str() else {
                return frame;
            };
            let result = match method {
                "fs/read_text_file" => json!({"content": "second line\nthird line\n"}),
                "fs/write_text_file" => json!({}),
                "terminal/create" => json!({"terminalId": "terminal-1"}),
                "terminal/output" => json!({
                    "output": "hello",
                    "truncated": false,
                    "exitStatus": {"exitCode": 0, "signal": null},
                }),
                "terminal/wait_for_exit" => json!({"exitCode": 0, "signal": null}),
                "terminal/kill" | "terminal/release" => json!({}),
                "session/request_permission" => {
                    let option = frame["params"]["options"]
                        .as_array()
                        .and_then(|options| {
                            options.iter().find(|option| option["kind"] == permission)
                        })
                        .unwrap_or_else(|| panic!("no {permission} option in {frame}"));
                    json!({"outcome": {"outcome": "selected", "optionId": option["optionId"]}})
                }
                other => panic!("unexpected daemon request {other}: {frame}"),
            };
            self.reply(&frame, result).await;
        }
    }
}

async fn initialize(peer: &mut WirePeer, id: i64, capabilities: Value) -> Value {
    peer.request(
        json!(id),
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": capabilities,
            "clientInfo": client_info(),
        }),
    )
    .await
}

async fn new_session(peer: &mut WirePeer, id: i64, cwd: &Path) -> String {
    let response = peer
        .request(
            json!(id),
            "session/new",
            json!({"cwd": cwd, "mcpServers": []}),
        )
        .await;
    response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new failed: {response}"))
        .to_string()
}

fn prompt(session: &str, text: &str) -> Value {
    json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]})
}

/// Replaces values that depend on the host or the run, and sorts object keys
/// so the golden files read the same whichever map order serde_json uses.
struct Mask {
    roots: Vec<(String, &'static str)>,
}

impl Mask {
    fn new(cwd: &Path) -> Self {
        let temp = std::env::temp_dir();
        let mut roots = vec![
            (cwd.display().to_string(), "<cwd>"),
            (temp.display().to_string(), "<tmp>"),
        ];
        if let Ok(canonical) = temp.canonicalize() {
            roots.push((canonical.display().to_string(), "<tmp>"));
        }
        for (root, _) in &mut roots {
            while root.len() > 1 && root.ends_with(std::path::MAIN_SEPARATOR) {
                root.pop();
            }
        }
        // Longer roots first: the cwd lives inside the temp directory.
        roots.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        Self { roots }
    }

    fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item)).collect())
            }
            Value::Object(entries) => {
                let mut entries = entries.clone();
                // Readiness warnings depend on the host's sandbox, and they are
                // Orca's own metadata rather than anything the SDK shapes.
                if let Some(Value::Object(meta)) = entries.get_mut("_meta") {
                    meta.remove("orca.dev/readiness");
                    if meta.is_empty() {
                        entries.remove("_meta");
                    }
                }
                let mut keys = entries.keys().collect::<Vec<_>>();
                keys.sort();
                let mut sorted = Map::new();
                for key in keys {
                    let value = if key.ends_with("_unix_ms") && entries[key].is_number() {
                        Value::from("<unix-ms>")
                    } else {
                        self.value(&entries[key])
                    };
                    sorted.insert(key.clone(), value);
                }
                Value::Object(sorted)
            }
            other => other.clone(),
        }
    }

    fn text(&self, text: &str) -> String {
        let mut text = text.to_string();
        let mut rooted = false;
        for (root, token) in &self.roots {
            if text.contains(root.as_str()) {
                text = text.replace(root.as_str(), token);
                rooted = true;
            }
        }
        if rooted {
            text = text.replace('\\', "/");
        }
        mask_ids_and_times(&text)
    }
}

/// UUIDs become `<uuid>`; RFC 3339 timestamps become `<time>`.
fn mask_ids_and_times(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(length) = uuid_at(&bytes[index..]) {
            output.push_str("<uuid>");
            index += length;
        } else if let Some(length) = timestamp_at(&bytes[index..]) {
            output.push_str("<time>");
            index += length;
        } else {
            let next = text[index..].chars().next().unwrap();
            output.push(next);
            index += next.len_utf8();
        }
    }
    output
}

fn uuid_at(bytes: &[u8]) -> Option<usize> {
    const SHAPE: &[u8; 36] = b"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
    let candidate = bytes.get(..SHAPE.len())?;
    candidate
        .iter()
        .zip(SHAPE)
        .all(|(byte, shape)| match shape {
            b'-' => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        .then_some(SHAPE.len())
}

/// `YYYY-MM-DDTHH:MM:SS`, an optional fraction, then an optional `Z` or `±HH:MM`.
fn timestamp_at(bytes: &[u8]) -> Option<usize> {
    const SHAPE: &[u8; 19] = b"dddd-dd-ddTdd:dd:dd";
    let candidate = bytes.get(..SHAPE.len())?;
    let matches = candidate
        .iter()
        .zip(SHAPE)
        .all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            _ => byte == shape,
        });
    if !matches {
        return None;
    }
    let mut length = SHAPE.len();
    if bytes.get(length) == Some(&b'.') {
        length += 1;
        while bytes.get(length).is_some_and(u8::is_ascii_digit) {
            length += 1;
        }
    }
    match bytes.get(length..) {
        Some([b'Z', ..]) => length += 1,
        Some([b'+' | b'-', h1, h2, b':', m1, m2, ..])
            if [h1, h2, m1, m2].iter().all(|digit| digit.is_ascii_digit()) =>
        {
            length += 6
        }
        _ => {}
    }
    Some(length)
}

/// What each peer received, masked. Other tests in the same process can save
/// sessions in the shared test home, so a `session/list` result keeps only
/// this scenario's directory and drops the page cursor that depends on them.
fn transcript(peers: &[&WirePeer], cwd: &Path) -> Value {
    let mask = Mask::new(cwd);
    Value::Array(
        peers
            .iter()
            .map(|peer| {
                let mut responses = Vec::new();
                let mut requests = Vec::new();
                let mut notifications = Vec::new();
                for frame in &peer.frames {
                    let mut frame = mask.value(frame);
                    match (frame.get("method").is_some(), frame.get("id").is_some()) {
                        (false, _) => {
                            let method = peer.methods.get(&frame["id"].to_string());
                            if method.is_some_and(|method| method == "session/list")
                                && let Some(result) = frame["result"].as_object_mut()
                            {
                                result.remove("nextCursor");
                                if let Some(sessions) =
                                    result.get_mut("sessions").and_then(Value::as_array_mut)
                                {
                                    sessions.retain(|session| session["cwd"] == "<cwd>");
                                }
                            }
                            responses.push(frame);
                        }
                        (true, true) => requests.push(frame),
                        (true, false) => notifications.push(frame),
                    }
                }
                notifications.sort_by_key(Value::to_string);
                // Keys in sorted order, so the file reads the same whichever map
                // order serde_json uses.
                json!({
                    "clientRequests": requests,
                    "notifications": notifications,
                    "responses": responses,
                })
            })
            .collect(),
    )
}

fn assert_wire_golden(name: &str, actual: Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/golden/acp_wire")
        .join(format!("{name}.json"));
    let text = format!("{}\n", serde_json::to_string_pretty(&actual).unwrap());
    if std::env::var_os("ORCA_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).expect("write wire golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "wire golden `{name}` is missing at {}; rerun with ORCA_UPDATE_GOLDEN=1 set \
             (e.g. `ORCA_UPDATE_GOLDEN=1 cargo nextest run -p orca-runtime --lib -E \
             'test(/wire_baseline/)'`) to generate it",
            path.display()
        )
    });
    let expected: Value = serde_json::from_str(&expected).expect("wire golden is JSON");
    assert!(
        expected == actual,
        "wire golden `{name}` differs from what the daemon sent; review and rerun with \
         ORCA_UPDATE_GOLDEN=1 to accept. The daemon sent:\n{text}"
    );
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    (directory, cwd)
}

#[test]
fn wire_baseline_stdio_session_lifecycle() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        initialize(
            &mut peer,
            1,
            json!({"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}),
        )
        .await;
        peer.request(json!(2), "authenticate", json!({"methodId": "none"}))
            .await;
        let session = new_session(&mut peer, 3, &cwd).await;
        peer.request(json!(4), "session/list", json!({"cwd": cwd}))
            .await;
        // A string id, then a cancel for it after its response: nothing answers a notification.
        peer.request(
            json!("prompt-1"),
            "session/prompt",
            prompt(&session, "complete"),
        )
        .await;
        peer.notify("$/cancel_request", json!({"requestId": "prompt-1"}))
            .await;
        peer.request(json!(5), "orca.dev/unknown", json!({})).await;
        initialize(&mut peer, 11, json!({})).await;
        peer.request(
            json!(6),
            "session/set_mode",
            json!({"sessionId": session, "modeId": "plan"}),
        )
        .await;
        peer.request(
            json!(7),
            "session/load",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
        )
        .await;
        let queue = peer
            .request(
                json!(8),
                "orca.dev/session/queue/list",
                json!({"sessionId": session}),
            )
            .await;
        let revision = queue["result"]["revision"].clone();
        let paused = peer
            .request(
                json!(9),
                "orca.dev/session/queue/pause",
                json!({"sessionId": session, "expectedRevision": revision}),
            )
            .await;
        peer.request(
            json!(10),
            "orca.dev/session/queue/add",
            json!({
                "sessionId": session,
                "expectedRevision": paused["result"]["revision"],
                "input": "queued later",
            }),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_session_lifecycle", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_stdio_malformed_params() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        peer.request(json!(1), "initialize", json!("not-an-object"))
            .await;
        peer.request(
            json!(2),
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": client_info()}),
        )
        .await;
        peer.request(json!(3), "authenticate", json!("not-an-object"))
            .await;
        peer.request(json!(4), "authenticate", json!({"methodId": 42}))
            .await;
        peer.request(json!(5), "session/new", json!("not-an-object"))
            .await;
        peer.request(
            json!(6),
            "session/new",
            json!({"cwd": 42, "mcpServers": []}),
        )
        .await;
        peer.request(json!(7), "session/load", json!("not-an-object"))
            .await;
        peer.request(
            json!(8),
            "session/load",
            json!({"sessionId": 42, "cwd": cwd, "mcpServers": []}),
        )
        .await;
        let session = new_session(&mut peer, 9, &cwd).await;
        peer.request(json!(10), "session/prompt", json!("not-an-object"))
            .await;
        peer.request(json!(11), "session/prompt", json!({"sessionId": session}))
            .await;
        peer.request(json!(12), "session/set_mode", json!({})).await;
        peer.request(
            json!(13),
            "session/set_config_option",
            json!({"sessionId": 42}),
        )
        .await;
        peer.request(
            json!(14),
            "session/set_config_option",
            json!({"sessionId": session, "configId": "reasoning", "value": 5}),
        )
        .await;
        peer.request(json!(15), "session/set_model", json!({}))
            .await;
        peer.request(json!(16), "session/list", json!({"cursor": 5}))
            .await;
        peer.request(
            json!(17),
            "session/list",
            json!({"cwd": cwd, "cursor": "x"}),
        )
        .await;
        peer.request(
            json!(18),
            "session/list",
            json!({"cwd": cwd, "additionalDirectories": [cwd]}),
        )
        .await;
        peer.request(
            json!(19),
            "orca.dev/session/queue/list",
            json!("not-an-object"),
        )
        .await;
        peer.request(
            json!(20),
            "orca.dev/session/queue/pause",
            json!({"sessionId": session}),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_malformed_params", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_stdio_cancel() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(WaitForCancelExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.start(json!(3), "session/prompt", prompt(&session, "wait"))
            .await;
        peer.notify("session/cancel", json!({"sessionId": session}))
            .await;
        peer.response(&json!(3)).await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_cancel", transcript(&[&peer], &cwd));
    });
}

/// Runs one prompt whose executor calls back into the client, answering
/// every daemon request with a canned result.
async fn client_capability_scenario(name: &str, host: RuntimeHost, permission: &str) {
    let (_directory, cwd) = workspace();
    let mut endpoint = Endpoint::stdio(host, &cwd);
    let mut peer = endpoint.connect();
    initialize(
        &mut peer,
        1,
        json!({"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}),
    )
    .await;
    let session = new_session(&mut peer, 2, &cwd).await;
    peer.start(json!(3), "session/prompt", prompt(&session, "go"))
        .await;
    peer.serve_client(&json!(3), permission).await;
    peer.hang_up().await;
    endpoint.shut_down().await;
    assert_wire_golden(name, transcript(&[&peer], &cwd));
}

#[test]
fn wire_baseline_stdio_reads_a_file_through_the_client() {
    run(async {
        let (content_tx, _content_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
            .unwrap();
        client_capability_scenario("stdio_read_text_file", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_writes_a_file_through_the_client() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
            .unwrap();
        client_capability_scenario("stdio_write_text_file", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_runs_a_terminal_through_the_client() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(TerminalObserveExecutor { outcome_tx }))
                .unwrap();
        client_capability_scenario("stdio_terminal", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_asks_the_client_for_permission() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(StandardInteractionExecutor {
            behaviors: Mutex::new(vec![StandardInteractionBehavior::ToolApproval]),
            outcome_tx,
        }))
        .unwrap();
        client_capability_scenario("stdio_permission", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_prompt_with_tools() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::stdio(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.request(
            json!(3),
            "session/prompt",
            prompt(&session, "plan Inspect references"),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_prompt_with_tools", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_daemon_session_settings() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::daemon(host, &cwd);
        let mut owner = endpoint.connect();
        initialize(&mut owner, 1, json!({})).await;
        let session = new_session(&mut owner, 2, &cwd).await;
        owner
            .request(
                json!(3),
                "session/set_model",
                json!({"sessionId": session, "modelId": "deepseek-v4-pro"}),
            )
            .await;
        owner
            .request(
                json!(4),
                "session/set_mode",
                json!({"sessionId": session, "modeId": "plan"}),
            )
            .await;
        owner
            .request(
                json!(5),
                "session/set_config_option",
                json!({"sessionId": session, "configId": "reasoning", "value": "high"}),
            )
            .await;
        owner
            .request(
                json!(6),
                "session/set_config_option",
                json!({"sessionId": session, "configId": "model", "value": " padded "}),
            )
            .await;
        owner
            .request(
                json!(7),
                "session/set_model",
                json!({"sessionId": session, "modelId": " padded "}),
            )
            .await;
        owner
            .request(json!(8), "session/list", json!({"cwd": cwd}))
            .await;
        let mut second = endpoint.connect();
        initialize(&mut second, 1, json!({})).await;
        second
            .request(
                json!(2),
                "session/load",
                json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        owner.hang_up().await;
        second.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden(
            "daemon_session_settings",
            transcript(&[&owner, &second], &cwd),
        );
    });
}

#[test]
fn wire_baseline_daemon_prompt_projection() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::daemon(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(
            &mut peer,
            1,
            json!({"_meta": {"orca.dev/projection": {"version": 1}}}),
        )
        .await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.seen(|frame| frame["params"]["_meta"]["orca.dev/projection"]["phase"] == "ready")
            .await;
        peer.request(json!(3), "session/prompt", prompt(&session, "mock_usage"))
            .await;
        peer.request(
            json!(4),
            "session/prompt",
            prompt(&session, "plan Inspect references"),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("daemon_prompt_projection", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_daemon_declined_permission_is_a_refusal() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::daemon(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        // Full-auto would grant the permission without asking the client.
        peer.request(
            json!(3),
            "session/set_mode",
            json!({"sessionId": session, "modeId": "suggest"}),
        )
        .await;
        let extra = cwd.join("extra");
        peer.start(
            json!(4),
            "session/prompt",
            prompt(
                &session,
                &format!(
                    "request_permissions_then_bash {} :: printf hi",
                    extra.display()
                ),
            ),
        )
        .await;
        peer.serve_client(&json!(4), "reject_once").await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("daemon_declined_permission", transcript(&[&peer], &cwd));
    });
}
