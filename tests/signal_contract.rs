//! Signal contract: headless runs must cancel their work, not die on the signal.
//!
//! `orca exec` used to keep the default disposition for SIGINT/SIGTERM, so the
//! process died mid-turn: task-owned commands kept running and the JSONL stream
//! ended without `session.completed`. These tests interrupt a live turn and
//! assert the graceful outcome — terminal record, stopped child, conventional
//! exit code.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::{TempDir, tempdir};

/// Long enough that the turn is still running when the signal arrives.
const SLEEP_SECONDS: u64 = 60;

struct Fixture {
    _home: TempDir,
    cwd: TempDir,
    pid_file: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let cwd = tempdir().expect("temporary cwd");
        let pid_file = cwd.path().join("child.pid");
        Self {
            _home: tempdir().expect("temporary ORCA_HOME"),
            cwd,
            pid_file,
        }
    }

    /// The mock provider drives one bash tool call that records its own shell
    /// pid and then blocks, which makes the child observable from the test.
    fn prompt(&self) -> String {
        format!(
            "mock_stream_tool_delay_ms 0 bash echo $$ > {}; sleep {SLEEP_SECONDS}",
            self.pid_file.display()
        )
    }
}

fn spawn_orca(fixture: &Fixture) -> Child {
    Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--provider",
            "mock",
            "--mode",
            "full-auto",
            "--output-format",
            "jsonl",
            "--no-history",
            "--cwd",
        ])
        .arg(fixture.cwd.path())
        .arg("--")
        .arg(fixture.prompt())
        .env("ORCA_HOME", fixture._home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca")
}

/// Wait until the tool command reports its pid, so the signal lands mid-turn.
fn wait_for_child_pid(fixture: &Fixture) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = fs::read_to_string(&fixture.pid_file)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the tool command never started: {} is still empty",
            fixture.pid_file.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn process_is_alive(pid: i32) -> bool {
    // `kill -0` succeeds exactly while the pid exists and is signalable.
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

fn run_interrupted(signal: &str, expected_exit_code: i32) {
    let fixture = Fixture::new();
    let child = spawn_orca(&fixture);
    let tool_pid = wait_for_child_pid(&fixture);
    assert!(
        process_is_alive(tool_pid),
        "the tool command should be running before {signal}"
    );

    Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .expect("send signal");

    let output = child.wait_with_output().expect("wait for orca");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(expected_exit_code),
        "{signal} should keep the conventional exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let terminal = events
        .last()
        .unwrap_or_else(|| panic!("{signal} produced no events: {stdout}"));
    assert_eq!(
        terminal["type"], "session.completed",
        "{signal} must commit a terminal record; got: {terminal}"
    );
    assert_eq!(
        terminal["payload"]["status"], "cancelled",
        "{signal} must report the interrupted session as cancelled"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_alive(tool_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survived = process_is_alive(tool_pid);
    if survived {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(tool_pid.to_string())
            .status();
    }
    assert!(
        !survived,
        "{signal} left the task-owned command (pid {tool_pid}) running"
    );
}

#[test]
fn sigint_cancels_the_running_command_and_commits_a_terminal() {
    run_interrupted("INT", 130);
}

#[test]
fn sigterm_cancels_the_running_command_and_commits_a_terminal() {
    run_interrupted("TERM", 143);
}
