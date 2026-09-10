//! Production process boundaries: daemon, independent stdio bridges, and headless attach.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Process(Child);

impl Process {
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().expect("poll child") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "child {} did not exit",
                self.0.id()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate(&mut self) {
        // SAFETY: PID belongs to this live, owned child; never read from a stale PID file.
        assert_eq!(unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) }, 0);
        assert!(self.wait().success());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("test directory");
        let root = temp
            .path()
            .canonicalize()
            .expect("canonical test directory");
        let cwd = root.join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        Self {
            _temp: temp,
            cwd,
            home: root.join("home"),
            socket: root.join("ipc/daemon.sock"),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command
            .env("ORCA_HOME", &self.home)
            .env_remove("ORCA_API_KEY")
            .env_remove("DEEPSEEK_API_KEY")
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        command
    }

    fn daemon_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["--provider", "mock", "--mode", "suggest", "daemon", "--cwd"])
            .arg(&self.cwd)
            .arg("--socket")
            .arg(&self.socket);
        command
    }

    fn daemon(&self) -> Process {
        let mut daemon = Process(self.daemon_command().spawn().expect("daemon process"));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited before accepting clients"
            );
            if std::os::unix::net::UnixStream::connect(&self.socket).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon socket never became ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn bridge(&self, extension: bool) -> WireClient {
        self.bridge_with_capabilities(if extension {
            json!({"_meta":{"orca.dev/projection":{"version":1}}})
        } else {
            json!({})
        })
    }

    fn bridge_with_capabilities(&self, capabilities: Value) -> WireClient {
        let mut child = self
            .command()
            .arg("acp-bridge")
            .arg("--socket")
            .arg(&self.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("bridge process");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.expect("bridge stdout");
                let value: Value = serde_json::from_str(&line).expect("ACP JSON only on stdout");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut client = WireClient {
            process: Process(child),
            input: Some(input),
            output: rx,
            reader: Some(reader),
            frames: vec![],
        };
        let response = client.request(
            1,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":capabilities}),
        );
        assert_eq!(response["result"]["protocolVersion"], 1);
        client
    }
}

struct WireClient {
    process: Process,
    input: Option<ChildStdin>,
    output: Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    frames: Vec<Value>,
}

impl WireClient {
    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().expect("connected bridge input");
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    }

    fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = self.output.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!("ACP read failed: {error}; frames: {:?}", self.frames)
            });
            let matched = predicate(&frame);
            self.frames.push(frame.clone());
            if matched {
                return frame;
            }
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        self.until(|frame| frame["id"] == id)
    }

    fn new_session(&mut self, cwd: &Path) -> String {
        let response = self.request(2, "session/new", json!({"cwd":cwd,"mcpServers":[]}));
        response["result"]["sessionId"]
            .as_str()
            .expect("new session ID")
            .to_string()
    }

    fn load(&mut self, cwd: &Path, id: &str) {
        let response = self.request(
            2,
            "session/load",
            json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}),
        );
        assert!(response.get("result").is_some(), "{response}");
    }

    fn prompt(&mut self, id: u64, session: &str, text: &str) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
            "params":{"sessionId":session,"prompt":[{"type":"text","text":text}]}}));
    }

    fn assistant_text(&self) -> String {
        self.frames
            .iter()
            .filter(|frame| frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
            .collect()
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn two_plain_clients_share_live_turn_disconnect_reload_and_restart_without_duplicate_history() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let mut owner = fixture.bridge(false);
    let session = owner.new_session(&fixture.cwd);
    let mut observer = fixture.bridge(false);
    observer.load(&fixture.cwd, &session);
    let marker = fixture.cwd.join("complete-shared-turn");
    owner.prompt(
        3,
        &session,
        &format!("mock_stream_release_marker {}", marker.display()),
    );
    owner.until(|frame| {
        frame["params"]["update"]["content"]["text"] == "Mock release-marker stream started."
    });
    observer.until(|frame| {
        frame["params"]["update"]["content"]["text"] == "Mock release-marker stream started."
    });
    observer.prompt(3, &session, "must not be admitted");
    let rejected = observer.until(|frame| frame["id"] == 3);
    assert!(rejected.get("error").is_some(), "{rejected}");
    drop(owner);

    let mut reattached = fixture.bridge(true);
    reattached.load(&fixture.cwd, &session);
    std::fs::write(marker, b"complete").unwrap();
    reattached
        .until(|frame| frame["params"]["_meta"]["orca.dev/projection"]["phase"] == "terminal");
    observer.until(|frame| {
        frame["params"]["update"]["content"]["text"] == "Mock release-marker stream completed."
    });
    assert_eq!(
        observer.assistant_text(),
        "Mock release-marker stream started.Mock release-marker stream completed."
    );
    assert!(
        observer
            .frames
            .iter()
            .filter(|frame| frame["method"] == "session/update")
            .all(|frame| frame["params"].get("_meta").is_none()),
        "plain ACP must not depend on extension metadata"
    );
    let mut reload = fixture.bridge(false);
    reload.load(&fixture.cwd, &session);
    assert_eq!(reload.assistant_text(), observer.assistant_text());
    drop(reload);
    drop(reattached);
    drop(observer);
    daemon.terminate();
    assert!(
        !fixture.socket.exists(),
        "graceful shutdown removes only its socket"
    );

    let mut restarted = fixture.daemon();
    let mut loaded = fixture.bridge(false);
    loaded.load(&fixture.cwd, &session);
    assert_eq!(
        loaded.assistant_text(),
        "Mock release-marker stream started.Mock release-marker stream completed."
    );
    let listing = loaded.request(4, "session/list", json!({"cwd":fixture.cwd}));
    assert!(
        listing["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["sessionId"] == session)
    );
    drop(loaded);
    restarted.terminate();
}

#[test]
fn shared_prompt_response_is_after_all_updates_and_headless_uses_same_session() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let mut client = fixture.bridge(false);
    let session = client.new_session(&fixture.cwd);
    client.prompt(3, &session, "mock_stream_delay_ms 100");
    let response = client.until(|frame| frame["id"] == 3);
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        client.assistant_text(),
        "Mock slow stream started.Mock slow stream completed."
    );
    let output = fixture
        .command()
        .arg("attach")
        .arg(&session)
        .arg("--socket")
        .arg(&fixture.socket)
        .arg("--cwd")
        .arg(&fixture.cwd)
        .args(["--exec", "mock_stream_delay_ms 100"])
        .stdout(Stdio::piped())
        .output()
        .expect("headless attachment");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.matches("Mock slow stream completed.").count(),
        2,
        "history plus one new turn"
    );
    drop(client);
    daemon.terminate();
}

#[test]
fn endpoint_is_private_singleton_and_refuses_unsafe_stale_paths() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    assert_eq!(
        std::fs::metadata(&fixture.socket).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(fixture.socket.parent().unwrap())
            .unwrap()
            .mode()
            & 0o777,
        0o700
    );
    let mut loser = Process(fixture.daemon_command().spawn().unwrap());
    assert!(!loser.wait().success());
    assert!(std::os::unix::net::UnixStream::connect(&fixture.socket).is_ok());
    daemon.0.kill().unwrap();
    daemon.wait();
    assert!(fixture.socket.exists(), "crash leaves a stale socket");
    let mut recovered = fixture.daemon();
    recovered.terminate();
    std::fs::write(&fixture.socket, b"do not delete").unwrap();
    let mut unsafe_path = Process(fixture.daemon_command().spawn().unwrap());
    assert!(!unsafe_path.wait().success());
    assert_eq!(std::fs::read(&fixture.socket).unwrap(), b"do not delete");
    std::fs::remove_file(&fixture.socket).unwrap();
    let target = fixture.socket.parent().unwrap().join("target");
    std::fs::write(&target, b"target survives").unwrap();
    std::os::unix::fs::symlink(&target, &fixture.socket).unwrap();
    let mut symlink = Process(fixture.daemon_command().spawn().unwrap());
    assert!(!symlink.wait().success());
    assert_eq!(std::fs::read(target).unwrap(), b"target survives");
    std::fs::remove_file(&fixture.socket).unwrap();
    std::fs::set_permissions(
        fixture.socket.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let mut public_dir = Process(fixture.daemon_command().spawn().unwrap());
    assert!(!public_dir.wait().success());
}

#[test]
fn shared_load_and_settings_reject_workspace_scope_and_policy_escalation() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let mut owner = fixture.bridge(false);
    let session = owner.new_session(&fixture.cwd);
    let other = tempfile::tempdir().unwrap();
    let mut attacker = fixture.bridge(false);
    let response = attacker.request(
        2,
        "session/load",
        json!({
            "sessionId":session,"cwd":other.path(),"mcpServers":[]
        }),
    );
    assert!(response.get("error").is_some());
    let response = attacker.request(
        3,
        "session/load",
        json!({
            "sessionId":session,"cwd":fixture.cwd,"mcpServers":[],
            "additionalDirectories":[other.path()]
        }),
    );
    assert!(response.get("error").is_some());
    let response = attacker.request(
        4,
        "session/load",
        json!({
            "sessionId":session,"cwd":fixture.cwd,
            "mcpServers":[{"name":"injected","command":"/bin/sh","args":[],"env":[]}]
        }),
    );
    assert!(response.get("error").is_some());
    let response = owner.request(
        3,
        "session/set_mode",
        json!({"sessionId":session,"modeId":"full-auto"}),
    );
    assert!(response.get("error").is_some());
    let response = owner.request(
        4,
        "session/set_config_option",
        json!({
            "sessionId":session,"configId":"cwd","value":other.path()
        }),
    );
    assert!(response.get("error").is_some());
    let response = owner.request(
        5,
        "session/set_model",
        json!({"sessionId":session,"modelId":"deepseek-v4-flash"}),
    );
    assert!(response.get("result").is_some(), "{response}");
    let response = owner.request(
        6,
        "session/set_config_option",
        json!({"sessionId":session,"configId":"mode","value":"plan"}),
    );
    let options = response["result"]["configOptions"]
        .as_array()
        .expect("settings result");
    assert!(
        options
            .iter()
            .any(|option| option["id"] == "mode" && option["currentValue"] == "plan")
    );
    assert!(
        options
            .iter()
            .any(|option| option["id"] == "model" && option["currentValue"] == "deepseek-v4-flash")
    );
    drop(attacker);
    drop(owner);
    daemon.terminate();
}

#[test]
fn process_restart_does_not_resume_an_interrupted_model_request() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let mut owner = fixture.bridge(false);
    let session = owner.new_session(&fixture.cwd);
    let marker = fixture.cwd.join("release");
    owner.prompt(
        3,
        &session,
        &format!("mock_stream_release_marker {}", marker.display()),
    );
    owner.until(|frame| {
        frame["params"]["update"]["content"]["text"] == "Mock release-marker stream started."
    });
    daemon.0.kill().unwrap();
    daemon.wait();
    drop(owner);
    let mut restarted = fixture.daemon();
    let mut reloaded = fixture.bridge(true);
    reloaded.load(&fixture.cwd, &session);
    std::fs::write(marker, b"release any accidentally resumed request").unwrap();
    assert!(!reloaded.assistant_text().contains("completed"));
    assert!(
        reloaded
            .output
            .recv_timeout(Duration::from_millis(400))
            .is_err(),
        "loading after restart must not execute another generation"
    );
    drop(reloaded);
    restarted.terminate();
}

#[test]
fn permission_is_routed_only_to_owner_and_disconnect_fails_closed() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let mut owner = fixture.bridge(false);
    let session = owner.new_session(&fixture.cwd);
    let mut observer = fixture.bridge(true);
    observer.load(&fixture.cwd, &session);
    owner.prompt(
        3,
        &session,
        "request_network_permissions_then_done example.invalid",
    );
    let request = owner.until(|frame| frame["method"] == "session/request_permission");
    assert_eq!(request["params"]["sessionId"], session);
    // Matching JSON-RPC IDs on another connection confer no authority.
    observer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{
        "outcome":{"outcome":"selected","optionId":"allow_always"}
    }}));
    drop(owner);
    let terminal = observer
        .until(|frame| frame["params"]["_meta"]["orca.dev/projection"]["phase"] == "terminal");
    assert_ne!(
        terminal["params"]["_meta"]["orca.dev/projection"]["stopReason"],
        "end_turn"
    );
    assert!(
        observer
            .frames
            .iter()
            .all(|frame| frame["method"] != "session/request_permission")
    );
    drop(observer);
    daemon.terminate();
}

#[test]
fn model_read_file_uses_local_disk_and_ignores_observer_resource_responses() {
    let fixture = Fixture::new();
    let mut daemon = fixture.daemon();
    let file = fixture.cwd.join("readme.txt");
    std::fs::write(&file, b"disk content").unwrap();
    let mut owner = fixture.bridge_with_capabilities(json!({"fs":{"readTextFile":true}}));
    let session = owner.new_session(&fixture.cwd);
    let mut observer = fixture.bridge(false);
    observer.load(&fixture.cwd, &session);
    // Ordinary model read_file uses disk, even when the client advertises ACP reads.
    // An unsolicited response must not be cached as content for the upcoming tool.
    observer.send(json!({"jsonrpc":"2.0","id":-1,"result":{"content":"wrong client"}}));
    let barrier = observer.request(4, "session/list", json!({"cwd":fixture.cwd}));
    assert!(barrier.get("result").is_some(), "{barrier}");
    owner.prompt(3, &session, &format!("read {}", file.display()));
    let result = owner.until(|frame| frame["id"] == 3);
    assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
    observer.until(|frame| {
        frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && frame["params"]["update"]["status"] == "completed"
    });
    let response = observer.request(5, "session/list", json!({"cwd":fixture.cwd}));
    assert!(response.get("result").is_some(), "{response}");
    for client in [&owner, &observer] {
        let request = client
            .frames
            .iter()
            .find(|frame| frame["params"]["update"]["sessionUpdate"] == "tool_call")
            .expect("read_file tool call");
        assert_eq!(request["params"]["update"]["kind"], "read");
        assert_eq!(
            request["params"]["update"]["rawInput"]["path"],
            file.display().to_string()
        );
        let results = client
            .frames
            .iter()
            .filter(|frame| {
                frame["params"]["update"]["sessionUpdate"] == "tool_call_update"
                    && frame["params"]["update"]["status"] == "completed"
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1, "one local read completion");
        assert_eq!(
            results[0]["params"]["update"]["toolCallId"],
            request["params"]["update"]["toolCallId"]
        );
        assert_eq!(
            results[0]["params"]["update"]["content"],
            json!([{"type":"content","content":{"type":"text","text":"disk content"}}])
        );
        assert!(
            client.frames.iter().all(|frame| {
                frame["method"] != "fs/read_text_file"
                    && !frame.to_string().contains("wrong client")
            }),
            "local read must not request or accept client-owned resource content"
        );
    }
    drop(observer);
    drop(owner);
    daemon.terminate();
}
