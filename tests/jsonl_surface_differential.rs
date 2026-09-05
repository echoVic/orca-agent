use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn released_v0_2_50_submit_wire_remains_byte_stable_after_identity_normalization() {
    let home = tempfile::tempdir().expect("create isolated ORCA_HOME");
    let workspace = tempfile::tempdir().expect("create isolated server workspace");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        Path::new("/"),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust fixture workspace");

    let output = run_fixture_until_terminal(home.path(), workspace.path());
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = parse_jsonl(&output.stdout);
    assert!(
        events
            .iter()
            .all(|event| event["event"] != "thread_started"),
        "stateless submit must not expose a recorded thread_started event"
    );

    let actual = normalize_dynamic_identity_tokens(&output.stdout, &events);
    let expected = include_bytes!("fixtures/jsonl-v0.2.50/expected-events.jsonl");
    assert_eq!(actual.as_slice(), expected);
    assert_orca_home_contains_only_folder_trust(home.path());
    assert_directory_is_empty(workspace.path(), "stateless server workspace");
}

#[test]
fn semantic_json_comparison_cannot_detect_wire_byte_drift() {
    let released: &[u8] = br#"{"event":"turn_completed","id":"fixture-submit","status":"success"}
"#;
    let reordered: &[u8] = br#"{"status":"success","id":"fixture-submit","event":"turn_completed"}
"#;
    let missing_trailing_newline = &released[..released.len() - 1];

    assert_ne!(released, reordered);
    assert_eq!(parse_jsonl(released), parse_jsonl(reordered));
    assert_ne!(released, missing_trailing_newline);
    assert_eq!(parse_jsonl(released), parse_jsonl(missing_trailing_newline));
}

fn run_fixture_until_terminal(home: &Path, workspace: &Path) -> Output {
    let mut child = ChildGuard::spawn(home, workspace);
    let mut stdin = child.child_mut().stdin.take().expect("JSONL server stdin");
    let stdout = child
        .child_mut()
        .stdout
        .take()
        .expect("JSONL server stdout");
    let stderr = child
        .child_mut()
        .stderr
        .take()
        .expect("JSONL server stderr");

    let (line_tx, line_rx) = mpsc::channel();
    let stdout_worker = thread::spawn(move || capture_stdout(stdout, line_tx));
    let stderr_worker = thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let terminal = stdin
        .write_all(include_bytes!("fixtures/jsonl-v0.2.50/requests.jsonl"))
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("write fixture requests: {error}"))
        .and_then(|()| wait_for_fixture_terminal(&line_rx));
    drop(stdin);

    let (status, exited_before_deadline) = child
        .wait_or_kill(FIXTURE_TIMEOUT)
        .expect("wait for JSONL server");
    let stdout = stdout_worker
        .join()
        .expect("JSONL stdout reader panicked")
        .expect("read JSONL server stdout");
    let stderr = stderr_worker
        .join()
        .expect("JSONL stderr reader panicked")
        .expect("read JSONL server stderr");

    if let Err(error) = terminal {
        panic!(
            "{error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }
    assert!(
        exited_before_deadline,
        "JSONL server did not exit within {FIXTURE_TIMEOUT:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );

    Output {
        status,
        stdout,
        stderr,
    }
}

fn capture_stdout(
    stdout: std::process::ChildStdout,
    line_tx: mpsc::Sender<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    let mut reader = BufReader::new(stdout);
    let mut captured = Vec::new();
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        captured.extend_from_slice(&line);
        let _ = line_tx.send(line);
    }
    Ok(captured)
}

fn wait_for_fixture_terminal(line_rx: &mpsc::Receiver<Vec<u8>>) -> Result<(), String> {
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "timed out after {FIXTURE_TIMEOUT:?} waiting for fixture-submit/turn_completed"
            ));
        }
        let line = line_rx
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => format!(
                    "timed out after {FIXTURE_TIMEOUT:?} waiting for fixture-submit/turn_completed"
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    "JSONL server stdout ended before fixture-submit/turn_completed".to_string()
                }
            })?;
        let event: Value = serde_json::from_slice(&line)
            .map_err(|error| format!("invalid JSONL server event: {error}"))?;
        if event["id"] == "fixture-submit" && event["event"] == "turn_completed" {
            return Ok(());
        }
        if event["id"] == "fixture-submit" && event["event"] == "error" {
            return Err(format!(
                "fixture submit failed before terminal event: {event}"
            ));
        }
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(home: &Path, workspace: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("ORCA_HOME", home)
            .current_dir(workspace)
            .args(["--mode", "server", "--provider", "mock"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn JSONL server");
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("JSONL server child")
    }

    fn wait_or_kill(&mut self, timeout: Duration) -> io::Result<(ExitStatus, bool)> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child_mut().try_wait()? {
                self.child.take();
                return Ok((status, true));
            }
            if Instant::now() >= deadline {
                let child = self.child_mut();
                let _ = child.kill();
                let status = child.wait()?;
                self.child.take();
                return Ok((status, false));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn parse_jsonl(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("JSONL server stdout must be UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid fixture JSONL"))
        .collect()
}

fn normalize_dynamic_identity_tokens(bytes: &[u8], events: &[Value]) -> Vec<u8> {
    let mut identities = HashMap::new();
    for event in events {
        record_identity(&mut identities, event.get("threadId"), "<thread>");
        record_identity(&mut identities, event.get("turnId"), "<turn>");
        record_identity(
            &mut identities,
            event.get("task").and_then(|task| task.get("task_id")),
            "<task>",
        );
        if let Some(item) = event.get("item") {
            let placeholder = match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => Some("<reasoning-item>"),
                Some("agent_message") => Some("<message-item>"),
                _ => None,
            };
            if let Some(placeholder) = placeholder {
                record_identity(&mut identities, item.get("id"), placeholder);
            }
        }
    }

    let mut replacements = identities
        .into_iter()
        .map(|(identity, placeholder)| {
            (
                serde_json::to_string(&identity).expect("serialize dynamic identity token"),
                serde_json::to_string(placeholder).expect("serialize placeholder token"),
            )
        })
        .collect::<Vec<_>>();
    replacements.sort_by(|(left, _), (right, _)| {
        right.len().cmp(&left.len()).then_with(|| left.cmp(right))
    });

    let mut normalized = std::str::from_utf8(bytes)
        .expect("JSONL server stdout must be UTF-8")
        .to_string();
    for (identity_token, placeholder_token) in replacements {
        assert!(
            normalized.contains(&identity_token),
            "recorded identity token disappeared before normalization: {identity_token}"
        );
        normalized = normalized.replace(&identity_token, &placeholder_token);
    }
    normalized.into_bytes()
}

fn record_identity(
    identities: &mut HashMap<String, &'static str>,
    value: Option<&Value>,
    placeholder: &'static str,
) {
    if let Some(value) = value.and_then(Value::as_str) {
        if let Some(existing) = identities.insert(value.to_string(), placeholder) {
            assert_eq!(
                existing, placeholder,
                "one dynamic identity cannot represent two wire concepts"
            );
        }
    }
}

fn assert_orca_home_contains_only_folder_trust(home: &Path) {
    let entries = directory_entries(home, "isolated ORCA_HOME");
    assert_eq!(
        entries,
        vec![
            "agent-events.jsonl".to_string(),
            "folder_trust.toml".to_string()
        ],
        "stateless submit may persist only trust and the agent lifecycle journal"
    );
}

fn assert_directory_is_empty(directory: &Path, description: &str) {
    let entries = directory_entries(directory, description);
    assert!(
        entries.is_empty(),
        "{description} must not contain runtime persistence artifacts: {entries:?}"
    );
}

fn directory_entries(directory: &Path, description: &str) -> Vec<String> {
    let mut entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {description}: {error}"))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("read {description} entry: {error}"))
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
