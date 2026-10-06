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

    assert_eq!(
        output.status.code(),
        Some(expected_exit_code),
        "{signal} should keep the conventional exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_cancelled_terminal(signal, &String::from_utf8_lossy(&output.stdout));

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

/// The JSONL stream ends with the run's terminal record: `session.completed`,
/// reporting the session as cancelled.
fn assert_cancelled_terminal(signal: &str, stdout: &str) {
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
}

#[test]
fn sigint_cancels_the_running_command_and_commits_a_terminal() {
    run_interrupted("INT", 130);
}

#[test]
fn sigterm_cancels_the_running_command_and_commits_a_terminal() {
    run_interrupted("TERM", 143);
}

/// `orca exec` with a stdio MCP server, configured in `fixture`'s ORCA_HOME,
/// that reads nothing for `SLEEP_SECONDS`: the run waits for it to connect.
/// Text output waits before the turn; JSONL output waits inside it. The
/// server writes its pid to `<cwd>/mcp.pid` after `pid_delay_seconds`. A
/// signal sent once the pid is there lands in that wait after a delay of two
/// seconds, and as early as the run starts its servers after none.
fn spawn_orca_with_a_slow_mcp_server(
    fixture: &Fixture,
    output_format: &str,
    pid_delay_seconds: u64,
) -> Child {
    let script = fixture.cwd.path().join("slow-mcp.sh");
    fs::write(
        &script,
        format!(
            "sleep {pid_delay_seconds}\nprintf '%s\\n' \"$$\" > \"$1/mcp.pid\"\nsleep {SLEEP_SECONDS}\nwhile IFS= read -r line; do :; done\n"
        ),
    )
    .expect("write the MCP server");
    fs::write(
        fixture._home.path().join("config.toml"),
        format!(
            "[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"/bin/sh\"\nargs = [{:?}, {:?}]\n",
            script.display().to_string(),
            fixture.cwd.path().display().to_string(),
        ),
    )
    .expect("configure the MCP server");
    Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--provider",
            "mock",
            "--output-format",
            output_format,
            "--no-history",
            "--cwd",
        ])
        .arg(fixture.cwd.path())
        .arg("--")
        .arg("hello")
        .env("ORCA_HOME", fixture._home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca")
}

/// The pid the MCP server wrote, looked for every `poll`.
fn wait_for_mcp_server_pid(fixture: &Fixture, poll: Duration) -> i32 {
    let pid_file = fixture.cwd.path().join("mcp.pid");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the MCP server never started");
        std::thread::sleep(poll);
    }
}

/// Whether the MCP server is still running a second after `orca` exited; a
/// survivor is killed, with its process group, before the test fails.
fn mcp_server_survived(server_pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_is_alive(server_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = process_is_alive(server_pid);
    if survived {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{server_pid}")])
            .status();
    }
    survived
}

fn sigint_while_waiting_for_mcp_servers_stops_them(output_format: &str) {
    let fixture = Fixture::new();
    let child = spawn_orca_with_a_slow_mcp_server(&fixture, output_format, 2);
    let server_pid = wait_for_mcp_server_pid(&fixture, Duration::from_millis(50));

    Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGINT");
    let output = child.wait_with_output().expect("wait for orca");

    // It was still connecting, and would have read nothing for a minute.
    let survived = mcp_server_survived(server_pid);
    assert_eq!(
        output.status.code(),
        Some(130),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !survived,
        "SIGINT left the MCP server (pid {server_pid}) running after orca exited"
    );
}

#[test]
fn sigint_during_the_mcp_startup_wait_stops_the_servers() {
    sigint_while_waiting_for_mcp_servers_stops_them("text");
}

#[test]
fn sigint_while_a_turn_waits_for_mcp_servers_stops_them() {
    sigint_while_waiting_for_mcp_servers_stops_them("jsonl");
}

/// A signal as soon as the run has started its MCP servers, which can be
/// before its signal handler was in effect when the handler was installed
/// only after the run's thread started.
#[test]
fn sigterm_while_mcp_servers_start_stops_them_and_commits_a_terminal() {
    let fixture = Fixture::new();
    let child = spawn_orca_with_a_slow_mcp_server(&fixture, "jsonl", 0);
    let server_pid = wait_for_mcp_server_pid(&fixture, Duration::from_millis(1));

    Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGTERM");
    let output = child.wait_with_output().expect("wait for orca");

    let survived = mcp_server_survived(server_pid);
    assert_eq!(
        output.status.code(),
        Some(143),
        "SIGTERM should keep the conventional exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !survived,
        "SIGTERM left the MCP server (pid {server_pid}) running after orca exited"
    );
    assert_cancelled_terminal("SIGTERM", &String::from_utf8_lossy(&output.stdout));
}
