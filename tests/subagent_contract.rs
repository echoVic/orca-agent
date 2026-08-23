use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;

use orca_core::task_types::TaskStatus;
use orca_runtime::tasks::TaskRegistry;

static SUBAGENT_CLI_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn foreground_cancel_stops_async_subagent_tree_only() {
    let registry = TaskRegistry::new("foreground-cancel-contract".to_string());
    let foreground = registry.create_main_session("foreground turn".to_string());
    registry.mark_running(&foreground.id).unwrap();
    let owned = registry.create_subagent_with_parent(
        "owned async subagent".to_string(),
        None,
        Some(foreground.id.clone()),
    );
    registry.mark_running(&owned.id).unwrap();
    let detached = registry.create_subagent("detached async subagent".to_string(), None);
    registry.mark_running(&detached.id).unwrap();

    let stopped = registry.request_stop_tree(&foreground.id).unwrap();

    assert!(stopped.contains(&owned.id));
    assert_eq!(
        registry.get(&owned.id).unwrap().status,
        TaskStatus::Stopping
    );
    assert_eq!(
        registry.get(&detached.id).unwrap().status,
        TaskStatus::Running
    );
}

#[test]
fn subagent_tool_runs_child_agent_and_emits_events() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "subagent");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "inspect repo");

    let started = find_event(&events, "subagent.started");
    assert_eq!(started["payload"]["id"], "mock-tool-1");
    assert_eq!(started["payload"]["description"], "inspect repo");
    assert_eq!(started["payload"]["task"]["kind"], "subagent");
    assert_eq!(started["payload"]["task"]["status"], "running");
    assert_eq!(
        started["payload"]["task"]["task_id"],
        "subagent-mock-tool-1:task-1"
    );

    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["id"], "mock-tool-1");
    assert_eq!(completed["payload"]["description"], "inspect repo");
    assert_eq!(completed["payload"]["task"]["kind"], "subagent");
    assert_eq!(completed["payload"]["task"]["status"], "succeeded");
    assert_eq!(completed["payload"]["task"]["turn"], 1);
    assert_eq!(
        completed["payload"]["task"]["task_id"],
        "subagent-mock-tool-1:task-1"
    );
    assert_eq!(completed["payload"]["status"], "success");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Mock runtime completed")
    );
    assert_eq!(completed["payload"]["error"], Value::Null);

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "completed");
    assert!(
        tool_completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Subagent status: success")
    );
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn async_subagent_launches_without_blocking_parent_tool() {
    let _guard = subagent_cli_test_guard();
    let cwd = tempdir().expect("temp cwd");
    let orca_home = tempdir().expect("temp orca home");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            "subagent async inspect repo",
        ])
        .output()
        .expect("run orca");

    assert!(
        output.status.success(),
        "async subagent launch failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let events = parse_jsonl(&output.stdout);
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "subagent.started"),
        "async launch must not create an unpaired foreground subagent lifecycle"
    );
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "subagent");
    assert_eq!(completed["payload"]["status"], "completed");

    let payload: Value =
        serde_json::from_str(completed["payload"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(payload["status"], "async_launched");
    let agent_id = payload["agent_id"].as_str().unwrap();
    assert!(agent_id.starts_with("task-"));
    assert_eq!(payload["description"], "inspect repo");
    let task_update = events
        .iter()
        .find(|event| {
            event["type"] == "task.status.updated" && event["payload"]["task"]["id"] == agent_id
        })
        .expect("async subagent task update");
    assert_eq!(task_update["payload"]["task"]["id"], agent_id);
    assert_eq!(task_update["payload"]["task"]["type"], "subagent");
    assert_eq!(task_update["payload"]["task"]["status"], "running");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");

    assert!(
        !cwd.path().join(".orca/task-sessions").exists(),
        "task sessions should not be written under project .orca"
    );
    let index_path = orca_home.path().join("task-sessions/task-index.json");
    let index: Value = serde_json::from_str(&std::fs::read_to_string(index_path).unwrap()).unwrap();
    assert!(index.get(agent_id).is_some());
}

#[test]
fn subagent_status_can_read_persisted_async_handle() {
    let _guard = subagent_cli_test_guard();
    let cwd = tempdir().expect("temp cwd");
    let orca_home = tempdir().expect("temp orca home");
    let launched = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            "subagent async inspect repo",
        ])
        .output()
        .expect("run orca");
    assert_launched_ok(&launched);
    let launch_events = parse_jsonl(&launched.stdout);
    let launch_completed = find_event(&launch_events, "tool.call.completed");
    let launch_payload: Value =
        serde_json::from_str(launch_completed["payload"]["output"].as_str().unwrap()).unwrap();
    let agent_id = launch_payload["agent_id"].as_str().unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            &format!("subagent_status {agent_id}"),
        ])
        .output()
        .expect("run orca");
    assert_eq!(status.status.code(), Some(0));
    let status_events = parse_jsonl(&status.stdout);
    let status_completed = find_event(&status_events, "tool.call.completed");
    assert_eq!(status_completed["payload"]["name"], "subagent_status");
    let status_payload: Value =
        serde_json::from_str(status_completed["payload"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(status_payload["agent_id"], agent_id);
    assert_eq!(status_payload["description"], "inspect repo");
    assert!(status_payload["status"].is_string());
}

#[test]
fn async_subagent_completes_after_launching_exec_process_exits() {
    let _guard = subagent_cli_test_guard();
    let cwd = tempdir().expect("temp cwd");
    let orca_home = tempdir().expect("temp orca home");
    write_sleep_hook_config(orca_home.path(), 0.4);
    let launched = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            "subagent async mock_usage",
        ])
        .output()
        .expect("run orca");
    assert_launched_ok(&launched);
    let launch_events = parse_jsonl(&launched.stdout);
    let launch_completed = find_event(&launch_events, "tool.call.completed");
    let launch_payload: Value =
        serde_json::from_str(launch_completed["payload"]["output"].as_str().unwrap()).unwrap();
    let agent_id = launch_payload["agent_id"].as_str().unwrap().to_string();

    let status_payload = poll_subagent_status(cwd.path(), orca_home.path(), &agent_id);

    assert_eq!(status_payload["agent_id"], agent_id);
    assert_eq!(status_payload["description"], "mock_usage");
    assert_eq!(status_payload["status"], "completed");
    assert_eq!(status_payload["task"]["kind"], "subagent");
    assert_eq!(status_payload["task"]["status"], "succeeded");
    assert_eq!(status_payload["task"]["turn"], 1);
    assert_eq!(
        status_payload["task"]["task_id"],
        format!("subagent-{agent_id}:task-1")
    );
    assert!(
        status_payload["output"]
            .as_str()
            .unwrap()
            .contains("Mock runtime completed with usage accounting")
    );
    assert_eq!(status_payload["usage"]["total_tokens"], 150);
}

#[test]
fn subagent_schema_accepts_matching_output() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent schema_ok",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["description"], "schema_ok");
    assert_eq!(completed["payload"]["status"], "success");
    assert_eq!(completed["payload"]["error"], Value::Null);

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn subagent_schema_failure_fails_parent_run() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent schema_fail",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["description"], "schema_fail");
    assert_eq!(completed["payload"]["status"], "failed");
    let error = completed["payload"]["error"].as_str().unwrap();
    assert!(error.contains("subagent output schema validation failed for schema_fail"));
    assert!(error.contains("$ expected object, got string"));

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

#[test]
fn subagent_batch_schema_failure_fails_parent_run() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent batch schema_fail",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .filter(|event| event["type"] == "subagent.completed")
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0]["payload"]["id"], "mock-tool-1");
    assert_eq!(completed[0]["payload"]["status"], "success");
    assert_eq!(completed[1]["payload"]["id"], "mock-tool-2");
    assert_eq!(completed[1]["payload"]["description"], "schema_fail");
    assert_eq!(completed[1]["payload"]["status"], "failed");
    let error = completed[1]["payload"]["error"].as_str().unwrap();
    assert!(error.contains("subagent output schema validation failed for schema_fail"));
    assert!(error.contains("$ expected object, got string"));

    let failed_tool = events
        .iter()
        .filter(|event| event["type"] == "tool.call.completed")
        .find(|event| event["payload"]["id"] == "mock-tool-2")
        .expect("failed batch tool completion");
    assert_eq!(failed_tool["payload"]["name"], "subagent");
    assert_eq!(failed_tool["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

#[test]
fn async_subagent_schema_failure_persists_failed_task() {
    let _guard = subagent_cli_test_guard();
    let cwd = tempdir().expect("temp cwd");
    let orca_home = tempdir().expect("temp orca home");
    let launched = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            "subagent async schema_fail",
        ])
        .output()
        .expect("run orca");
    assert_launched_ok(&launched);
    let launch_events = parse_jsonl(&launched.stdout);
    let launch_completed = find_event(&launch_events, "tool.call.completed");
    let launch_payload: Value =
        serde_json::from_str(launch_completed["payload"]["output"].as_str().unwrap()).unwrap();
    let agent_id = launch_payload["agent_id"].as_str().unwrap().to_string();

    let status_payload = poll_subagent_status_until_failed(cwd.path(), orca_home.path(), &agent_id);

    assert_eq!(status_payload["agent_id"], agent_id);
    assert_eq!(status_payload["description"], "schema_fail");
    assert_eq!(status_payload["status"], "failed");
    assert_eq!(status_payload["task"]["kind"], "subagent");
    assert_eq!(status_payload["task"]["status"], "failed");
    assert_eq!(status_payload["task"]["turn"], 1);
    let error = status_payload["error"].as_str().unwrap();
    assert!(error.contains("subagent output schema validation failed for schema_fail"));
    assert!(error.contains("$ expected object, got string"));
}

#[test]
fn nested_subagent_calls_are_rejected() {
    let _guard = subagent_cli_test_guard();
    let orca_home = tempdir().expect("temp orca home");
    std::fs::write(
        orca_home.path().join("config.toml"),
        "[subagents]\nmax_depth = 1\n",
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent subagent inner task",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("subagent max depth 1 reached")
    );

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

#[test]
fn default_subagent_depth_allows_one_nested_child() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent subagent inner task",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["status"], "success");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn worktree_isolated_subagent_writes_outside_parent_worktree() {
    let _guard = subagent_cli_test_guard();
    let repo = tempdir().expect("temp repo");
    run_git(repo.path(), &["init"]);
    run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
    run_git(repo.path(), &["config", "user.name", "Orca Test"]);
    std::fs::write(repo.path().join("file.txt"), "placeholder").expect("seed file");
    run_git(repo.path(), &["add", "file.txt"]);
    run_git(repo.path(), &["commit", "-m", "seed"]);

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(repo.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--mode",
            "full-auto",
            "subagent worktree edit file.txt :: placeholder => child",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "placeholder"
    );
    let worktrees = repo.path().join(".orca/worktrees");
    let changed_worktree = std::fs::read_dir(&worktrees)
        .expect("worktree directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("file.txt").exists())
        .expect("dirty worktree was preserved");
    assert_eq!(
        std::fs::read_to_string(changed_worktree.join("file.txt")).unwrap(),
        "child"
    );
}

#[test]
fn subagent_child_failure_fails_parent_run() {
    let _guard = subagent_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent mock_fail",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("mock child failure requested")
    );

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type}"))
}

fn subagent_cli_test_guard() -> MutexGuard<'static, ()> {
    // Recover from a poisoned lock so that a single failing test is reported on
    // its own merits instead of cascading into every other test failing with
    // `PoisonError`. The guard only serializes access; there is no shared state
    // to be left inconsistent by a panicking test.
    SUBAGENT_CLI_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn assert_launched_ok(output: &std::process::Output) {
    // Surface the child's captured streams so a Windows-only async launch
    // failure is diagnosable from CI instead of collapsing to `Some(1)`.
    assert_eq!(
        output.status.code(),
        Some(0),
        "orca exec exited non-zero\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn poll_subagent_status_until_failed(
    cwd: &std::path::Path,
    orca_home: &std::path::Path,
    agent_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_payload = None;
    while Instant::now() < deadline {
        let status = Command::new(env!("CARGO_BIN_EXE_orca"))
            .current_dir(cwd)
            .env("ORCA_HOME", orca_home)
            .args([
                "exec",
                "--output-format",
                "jsonl",
                "--provider",
                "mock",
                "--approval-mode",
                "full-auto",
                "--save-history",
                &format!("subagent_status {agent_id}"),
            ])
            .output()
            .expect("run orca");
        assert_eq!(status.status.code(), Some(0));
        let status_events = parse_jsonl(&status.stdout);
        let status_completed = find_event(&status_events, "tool.call.completed");
        let status_payload: Value =
            serde_json::from_str(status_completed["payload"]["output"].as_str().unwrap()).unwrap();
        if status_payload["status"] == "failed" {
            return status_payload;
        }
        assert_ne!(
            status_payload["status"], "completed",
            "async subagent completed despite schema mismatch: {status_payload}"
        );
        last_payload = Some(status_payload);
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "async subagent did not fail before timeout; last status: {}",
        last_payload
            .map(|payload| payload.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
}

fn poll_subagent_status(
    cwd: &std::path::Path,
    orca_home: &std::path::Path,
    agent_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_payload = None;
    while Instant::now() < deadline {
        let status = Command::new(env!("CARGO_BIN_EXE_orca"))
            .current_dir(cwd)
            .env("ORCA_HOME", orca_home)
            .args([
                "exec",
                "--output-format",
                "jsonl",
                "--provider",
                "mock",
                "--approval-mode",
                "full-auto",
                "--save-history",
                &format!("subagent_status {agent_id}"),
            ])
            .output()
            .expect("run orca");
        assert_eq!(status.status.code(), Some(0));
        let status_events = parse_jsonl(&status.stdout);
        let status_completed = find_event(&status_events, "tool.call.completed");
        let status_payload: Value =
            serde_json::from_str(status_completed["payload"]["output"].as_str().unwrap()).unwrap();
        if status_payload["status"] == "completed" {
            return status_payload;
        }
        assert_ne!(
            status_payload["status"], "failed",
            "async subagent failed before completion: {status_payload}"
        );
        last_payload = Some(status_payload);
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "async subagent did not complete before timeout; last status: {}",
        last_payload
            .map(|payload| payload.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
}

fn write_sleep_hook_config(home: &std::path::Path, seconds: f32) {
    std::fs::create_dir_all(home).expect("create ORCA_HOME");
    // The resolved host shell differs per platform, so the delay command has to
    // match its dialect. Non-Windows hosts run the config through `sh -c`, while Windows
    // resolves to PowerShell, where `sleep` is not a valid command.
    #[cfg(windows)]
    let command = format!(
        "Start-Sleep -Milliseconds {}",
        (seconds * 1000.0).round() as u64
    );
    #[cfg(not(windows))]
    let command = format!("sleep {seconds}");
    std::fs::write(
        home.join("config.toml"),
        format!("[[hooks]]\nevent = \"pre_model_call\"\ncommand = \"{command}\"\n"),
    )
    .expect("write hook config");
}
