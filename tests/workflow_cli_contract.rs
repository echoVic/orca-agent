use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;

fn trust_project(home: &std::path::Path, project: &std::path::Path) {
    orca_core::config::folder_trust::set_trust_with_config_dir(
        project,
        home,
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust workflow project");
}

#[test]
fn workflow_run_command_executes_script() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "async_launched");
    assert_eq!(value["workflowName"], "audit");
    wait_for_workflow_terminal_status(temp.path(), None, value["taskId"].as_str().unwrap());
}

#[test]
fn workflow_run_named_script_resolves_project_workflow() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let dir = temp.path().join(".orca/workflows");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("audit.js"),
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();
    trust_project(&home, temp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            "audit",
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["workflowName"], "audit");
    wait_for_workflow_terminal_status(temp.path(), Some(&home), value["taskId"].as_str().unwrap());
}

#[test]
fn disable_workflows_setting_blocks_launch() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("config.toml"), "disableWorkflows = true\n").unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default 'blocked';",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", temp.path())
        .args(["workflow", "run", script.to_str().unwrap()])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflows are disabled"));
}

#[test]
fn workflow_list_and_show_inspect_persisted_runs() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", temp.path().join("home"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    let home = temp.path().join("home");

    wait_for_workflow_terminal_status(temp.path(), Some(&home), task_id);

    let list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "list"])
        .output()
        .expect("list workflows");

    assert_eq!(list.status.code(), Some(0));
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    let runs = listed.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["taskId"], task_id);
    assert_eq!(runs[0]["runId"], run_id);
    assert_eq!(runs[0]["workflowName"], "audit");
    assert_eq!(runs[0]["status"], "completed");

    let show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "show", task_id])
        .output()
        .expect("show workflow");

    assert_eq!(show.status.code(), Some(0));
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["taskId"], task_id);
    assert_eq!(shown["runId"], run_id);
    assert_eq!(shown["workflowName"], "audit");
    assert_eq!(shown["status"], "completed");
}

#[test]
fn workflow_source_command_prints_saved_workflow_source() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let workflow_dir = temp.path().join(".orca/workflows");
    fs::create_dir_all(&workflow_dir).unwrap();
    let script = workflow_dir.join("audit.js");
    let source = "export const meta = { name: 'audit', description: 'Audit code', phases: ['scan'] };\nexport default await agent('inspect repo');";
    fs::write(&script, source).unwrap();
    trust_project(&home.join(".orca"), temp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("HOME", &home)
        .env("ORCA_HOME", home.join(".orca"))
        .args(["workflow", "source", "audit"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["name"], "audit");
    // The CLI reports the path as resolved from the working directory, which is
    // not run through `canonicalize`, so on Windows it lacks the `\\?\` verbatim
    // prefix and long-name expansion. Canonicalize both sides to compare the
    // same underlying file across platforms.
    let reported_path = std::path::Path::new(value["path"].as_str().unwrap());
    assert_eq!(
        reported_path.canonicalize().unwrap(),
        script.canonicalize().unwrap()
    );
    assert_eq!(value["meta"]["description"], "Audit code");
    assert_eq!(value["script"], source);
}

#[test]
fn workflow_run_returns_before_slow_workflow_completes() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let script = temp.path().join("slow.js");
    // The mock provider honors a `mock_stream_delay_ms <n>` prompt with a real,
    // cancellable in-provider delay, so the model call is deterministically slow
    // on every platform without depending on shell hooks or wall-clock races.
    const MODEL_CALL_MS: u64 = 6000;
    fs::write(
        &script,
        format!(
            "export const meta = {{ name: 'slow', description: 'Slow workflow', phases: [] }};\nexport default await agent('mock_stream_delay_ms {MODEL_CALL_MS}');"
        ),
    )
    .unwrap();

    let started = Instant::now();
    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");
    let elapsed = started.elapsed();

    assert_eq!(
        run.status.code(),
        Some(0),
        "workflow launch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    // This wall-clock bound only guards against a gross regression to fully
    // synchronous execution (launch blocking for the whole model call). The
    // real "returned while still active" guarantee is asserted deterministically
    // by wait_until_active below, so the deadline is expressed as a fraction of
    // the model-call delay with wide margin rather than a tight absolute value
    // that would race the orca binary's cold start.
    let blocking_guard = Duration::from_millis(MODEL_CALL_MS / 2);
    assert!(
        elapsed < blocking_guard,
        "workflow run blocked for {elapsed:?}; it must return well before the {MODEL_CALL_MS}ms model call completes"
    );
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();

    // The launching command returns while the model call is still streaming, so
    // the run is observably active. Poll for that state rather than assuming a
    // fixed startup latency.
    wait_until_active(temp.path(), Some(&home), task_id);

    wait_for_workflow_terminal_status(temp.path(), Some(&home), task_id);
    let completed = workflow_show(temp.path(), Some(&home), task_id);
    assert_eq!(
        completed["status"], "completed",
        "slow workflow did not complete: {completed}"
    );
}

#[test]
fn workflow_stop_requests_real_background_stop() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let script = temp.path().join("stoppable.js");
    // The first model call is deterministically slow via the mock provider's
    // in-provider delay, so the run is reliably active when we request the stop.
    fs::write(
        &script,
        "export const meta = { name: 'stoppable', description: 'Stoppable workflow', phases: [] };\nawait agent('mock_stream_delay_ms 6000');\nexport default await agent('second');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    // Poll until the run is active so the stop request provably lands on a live
    // task while the first model call is still streaming.
    wait_until_active(temp.path(), Some(&home), task_id);

    let stop = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "stop", task_id])
        .output()
        .expect("stop workflow");

    assert_eq!(
        stop.status.code(),
        Some(0),
        "stop failed: stderr={} show={}",
        String::from_utf8_lossy(&stop.stderr),
        workflow_show(temp.path(), Some(&home), task_id)
    );
    let stop_value: Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(stop_value["status"], "stop_requested");
    assert_eq!(stop_value["taskId"], task_id);
    assert_eq!(stop_value["runId"], run_id);

    wait_for_workflow_terminal_status(temp.path(), Some(&home), task_id);
    let stopped = workflow_show(temp.path(), Some(&home), task_id);
    assert_eq!(stopped["status"], "stopped");
}

#[test]
fn workflow_pause_resume_and_clone_control_persisted_run() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let script = temp.path().join("pausable.js");
    // Hold the first call at the workflow runner's native agent boundary so
    // pause is observed before any provider work can complete.
    fs::write(
        &script,
        "export const meta = { name: 'pausable', description: 'Pausable workflow', phases: [] };\nawait agent('first', { minHoldMs: 6000 });\nexport default await agent('second');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    // Wait until the worker has moved beyond the queued startup window before
    // requesting the pause. A persisted queued task can accept a pause request
    // before its worker reaches the pause barrier, allowing an immediate resume
    // to race worker startup on fast Windows runners.
    wait_for_workflow_status(temp.path(), Some(&home), task_id, "running");
    let pause = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "pause", task_id])
        .output()
        .expect("pause workflow");
    assert_eq!(
        pause.status.code(),
        Some(0),
        "pause failed: stderr={} show={}",
        String::from_utf8_lossy(&pause.stderr),
        workflow_show(temp.path(), Some(&home), task_id)
    );
    let pause_value: Value = serde_json::from_slice(&pause.stdout).unwrap();
    assert_eq!(pause_value["status"], "pause_requested");

    wait_for_workflow_status(temp.path(), Some(&home), task_id, "paused");

    let list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow", "list", "--name", "pausable", "--status", "paused",
        ])
        .output()
        .expect("list paused workflow");
    assert_eq!(list.status.code(), Some(0));
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let clone = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "clone", run_id])
        .output()
        .expect("clone workflow");
    assert_eq!(clone.status.code(), Some(0));
    let cloned: Value = serde_json::from_slice(&clone.stdout).unwrap();
    assert_eq!(cloned["status"], "draft_created");
    assert_eq!(cloned["workflowName"], "pausable");

    let resume = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "resume", run_id])
        .output()
        .expect("resume workflow");
    assert_eq!(resume.status.code(), Some(0));
    let resume_value: Value = serde_json::from_slice(&resume.stdout).unwrap();
    assert_eq!(resume_value["status"], "resume_requested");

    wait_for_workflow_terminal_status(temp.path(), Some(&home), task_id);
    let completed = workflow_show(temp.path(), Some(&home), task_id);
    assert_eq!(
        completed["status"], "completed",
        "resumed workflow did not complete: {completed}"
    );
}

#[test]
fn workflow_restart_commands_launch_from_persisted_run_record() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let script = temp.path().join("restartable.js");
    fs::write(
        &script,
        "export const meta = { name: 'restartable', description: 'Restartable workflow', phases: ['scan', 'review'] };\nconst scan = await phase('scan', async () => agent('first'));\nconst review = await phase('review', async () => agent('second'));\nexport default `${scan} ${review}`;",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();
    wait_for_workflow_terminal_status(temp.path(), Some(&home), task_id);

    let restart_failed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "restart-failed", run_id])
        .output()
        .expect("restart failed workflow agents");
    assert_eq!(restart_failed.status.code(), Some(0));
    let restarted: Value = serde_json::from_slice(&restart_failed.stdout).unwrap();
    assert_eq!(restarted["status"], "async_launched");
    assert_eq!(restarted["workflowName"], "restartable");
    let restarted_task = restarted["taskId"].as_str().unwrap();
    wait_for_workflow_terminal_status(temp.path(), Some(&home), restarted_task);

    let restart_phase = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args(["workflow", "restart-phase", run_id, "review"])
        .output()
        .expect("restart workflow phase");
    assert_eq!(restart_phase.status.code(), Some(0));
    let restarted: Value = serde_json::from_slice(&restart_phase.stdout).unwrap();
    assert_eq!(restarted["status"], "async_launched");
    assert_eq!(restarted["workflowName"], "restartable");
    wait_for_workflow_terminal_status(
        temp.path(),
        Some(&home),
        restarted["taskId"].as_str().unwrap(),
    );
}

#[test]
fn workflow_run_resume_from_run_id_rejects_cross_process_cache_resume() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("resumable.js");
    fs::write(
        &script,
        "export const meta = { name: 'resumable', description: 'Resumable workflow', phases: [] };\nexport default await agent('first');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let run_id = launched["runId"].as_str().unwrap();
    let task_id = launched["taskId"].as_str().unwrap();

    let resume = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--resume-from-run-id",
            run_id,
            script.to_str().unwrap(),
        ])
        .output()
        .expect("resume workflow from cache");

    assert!(
        !resume.status.success(),
        "standalone CLI resume should not reuse a persisted cache"
    );
    let stderr = String::from_utf8_lossy(&resume.stderr);
    assert!(
        stderr.contains("only available inside the active Orca session"),
        "unexpected stderr: {stderr}"
    );

    wait_for_workflow_terminal_status(temp.path(), None, task_id);
}

fn workflow_show(cwd: &std::path::Path, home: Option<&std::path::Path>, task_id: &str) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    command.current_dir(cwd);
    if let Some(home) = home {
        command.env("ORCA_HOME", home);
    }
    let output = command
        .args(["workflow", "show", task_id])
        .output()
        .expect("show workflow");

    assert_eq!(
        output.status.code(),
        Some(0),
        "workflow show failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_for_workflow_terminal_status(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    task_id: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_status = String::new();
    loop {
        let shown = workflow_show(cwd, home, task_id);
        let status = shown["status"].as_str().unwrap_or_default();
        if matches!(status, "completed" | "failed" | "stopped" | "cancelled") {
            return;
        }
        last_status.clear();
        last_status.push_str(status);
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "workflow task {task_id} did not reach a terminal state within 60s (last status: {last_status})"
    );
}

fn wait_for_workflow_status(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    task_id: &str,
    expected: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_status = String::new();
    loop {
        let shown = workflow_show(cwd, home, task_id);
        let status = shown["status"].as_str().unwrap_or_default();
        if status == expected {
            return;
        }
        last_status.clear();
        last_status.push_str(status);
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "workflow task {task_id} did not reach status {expected} within 60s (last status: {last_status})"
    );
}

/// Poll `workflow show` until the task is observably active (any non-terminal
/// status), returning the status seen. Fails if the task reaches a terminal
/// state or never reports a status within the deadline. This replaces fixed
/// sleeps that raced the background worker's startup on loaded CI runners.
fn wait_until_active(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    task_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let show = workflow_show(cwd, home, task_id);
        let status = show["status"].as_str().unwrap_or_default();
        if seen.last().map(String::as_str) != Some(status) {
            seen.push(status.to_string());
        }
        assert!(
            !matches!(status, "completed" | "failed" | "stopped" | "cancelled"),
            "workflow reached terminal state before it could be observed active \
             (status sequence: {seen:?}): {show}"
        );
        // Classify by exclusion, not by allowlist. `WorkflowRunStatus` has nine
        // variants; an allowlist of `queued`/`running` left the rest in a gap
        // where the loop neither returned nor failed, so it spun for the whole
        // run and only tripped once the run went terminal. Any non-terminal
        // status means the run is live, which is all these tests need before
        // issuing pause/stop. The status sequence is reported on failure so a
        // future gap names itself instead of looking like a timing flake.
        if !status.is_empty() {
            return show;
        }
        assert!(
            Instant::now() < deadline,
            "workflow task {task_id} never reported a status within 20s (last: {show})"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
