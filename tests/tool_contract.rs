use std::fs;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use serde_json::Value;

#[path = "support/sandbox_test_parent.rs"]
mod sandbox_test_support;

use sandbox_test_support::sandbox_test_parent;

static TOOL_CLI_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn read_file_emits_tool_request_and_completed_events() {
    let _guard = tool_cli_test_guard();
    let temp_dir = make_temp_workspace("read-file");
    fs::write(temp_dir.join("note.txt"), "orca read fixture\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "read note.txt",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "read_file");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "note.txt");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "read_file");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["truncated"], false);
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("orca read fixture")
    );
}

#[test]
fn git_status_emits_completed_tool_event() {
    let _guard = tool_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "git status",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "git_status");
    assert_eq!(requested["payload"]["action"], "read");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "git_status");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn grep_emits_completed_tool_event_with_matches() {
    let _guard = tool_cli_test_guard();
    let temp_dir = make_temp_workspace("grep");
    fs::write(
        temp_dir.join("fixture.txt"),
        "unique-orca-grep-fixture\nother line\n",
    )
    .expect("write grep fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        // The built-in grep must remain usable on a native Windows install
        // without requiring ripgrep to be present on PATH.
        .env("PATH", "")
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "grep unique-orca-grep-fixture",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "grep");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "unique-orca-grep-fixture");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "grep");
    assert_eq!(completed["payload"]["status"], "completed");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("fixture.txt")
    );
}

#[test]
fn suggest_denies_bash_in_jsonl_mode() {
    let _guard = tool_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "suggest",
            "bash printf hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));

    let events = parse_jsonl(&output.stdout);
    let resolved = find_event(&events, "approval.resolved");
    assert_eq!(resolved["payload"]["decision"], "deny");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "denied");
    assert_eq!(completed["payload"]["task"]["kind"], "shell");
    assert_eq!(completed["payload"]["task"]["status"], "approval_required");
}

#[test]
fn full_auto_allows_bash_tool() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("bash-home");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "hi");
}

#[test]
fn full_auto_bash_tool_events_include_shell_task_lifecycle() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("bash-lifecycle-home");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(requested["payload"]["task"]["kind"], "shell");
    assert_eq!(requested["payload"]["task"]["status"], "running");
    assert_eq!(requested["payload"]["task"]["turn"], 1);
    assert_eq!(completed["payload"]["task"]["kind"], "shell");
    assert_eq!(completed["payload"]["task"]["status"], "succeeded");
    assert_eq!(completed["payload"]["task"]["turn"], 1);
    assert_eq!(
        requested["payload"]["task"]["task_id"],
        completed["payload"]["task"]["task_id"]
    );
}

#[test]
fn full_auto_bash_persists_runtime_shell_task_record() {
    let _guard = tool_cli_test_guard();
    let workspace = make_temp_workspace("bash-shell-task");
    let home = make_temp_workspace("bash-shell-task-home");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--save-history",
            "--cwd",
            workspace.to_str().unwrap(),
            "bash printf persisted-shell-task",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "persisted-shell-task");

    let project_sessions_root = workspace.join(".orca").join("task-sessions");
    assert!(
        !project_sessions_root.exists(),
        "task sessions should not be written under project .orca"
    );

    let sessions_root = home.join("task-sessions");
    let task_files = find_task_files(&sessions_root);
    assert_eq!(task_files.len(), 1, "task files: {task_files:?}");
    let tasks: Value = serde_json::from_str(
        &fs::read_to_string(&task_files[0]).expect("read persisted shell tasks"),
    )
    .expect("persisted tasks json");
    let tasks = tasks.as_object().expect("persisted task map");
    let shell_task = tasks
        .values()
        .find(|task| task["task_type"] == "shell")
        .expect("persisted shell task");
    assert!(
        tasks
            .values()
            .any(|task| task["task_type"] == "main_session"),
        "headless execution should persist its canonical main-session task"
    );
    assert_eq!(shell_task["task_type"], "shell");
    assert_eq!(shell_task["status"], "completed");
    assert_eq!(shell_task["command"], "printf persisted-shell-task");
    assert_eq!(shell_task["result"], "persisted-shell-task");
}

#[test]
fn tool_output_truncation_policy_from_config_applies_to_bash() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("tool-truncation-home");
    fs::write(
        home.join("config.toml"),
        r#"
[tools]
output_truncation = { mode = "tokens", limit = 12 }
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf 'alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma'",
        ])
        .output()
        .expect("run orca");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["truncated"], true);
    let text = completed["payload"]["output"].as_str().unwrap();
    assert!(text.contains("Warning: truncated tool output"));
    assert!(text.contains("Original token count:"));

    let tasks = events
        .iter()
        .rev()
        .find(|event| event["type"] == "workflow.tasks.updated")
        .expect("complete task registry event");
    let main_task = tasks["payload"]["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["type"] == "main_session"))
        .expect("canonical main-session task");
    assert_eq!(main_task["outputTruncated"], true);
}

#[test]
fn pre_tool_hook_cannot_modify_approval_relevant_target_after_authority() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("hook-modify-home");
    fs::write(
        home.join("config.toml"),
        r#"
[[hooks]]
event = "pre_tool_use"
tool = "bash"
command = "printf '%s' '{\"action\":\"modify\",\"modified_target\":\"printf sanitized\"}'"
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf unsafe",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "failed");
    assert_eq!(
        completed["payload"]["error"],
        "pre-tool hook changed an approval-relevant request after its authority was evaluated"
    );
    assert!(completed["payload"]["output"].is_null());
}

#[test]
fn pre_tool_hook_json_deny_blocks_tool_even_when_exit_succeeds() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("hook-deny-home");
    fs::write(
        home.join("config.toml"),
        r#"
[[hooks]]
event = "pre_tool_use"
tool = "bash"
command = "printf '%s' '{\"action\":\"deny\",\"reason\":\"blocked by hook\"}'"
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf unsafe",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("blocked by hook")
    );
}

#[test]
fn full_auto_bash_writes_outside_workspace() {
    let _guard = tool_cli_test_guard();
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("tool-full-auto-deny-");
    let temp_dir = parent.path().join("workspace");
    std::fs::create_dir(&temp_dir).expect("workspace dir");
    let outside = parent.path().join(format!(
        "orca-outside-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--cwd",
            temp_dir.to_str().unwrap(),
            &format!("bash printf blocked > {}", outside.display()),
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        fs::read_to_string(&outside).expect("full-auto write"),
        "blocked"
    );

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
}

#[test]
fn request_permissions_grants_bash_write_root_for_current_turn() {
    let _guard = tool_cli_test_guard();
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("tool-request-permissions-");
    let workspace = parent.path().join("workspace");
    let extra = parent.path().join("extra");
    fs::create_dir(&workspace).expect("workspace dir");
    fs::create_dir(&extra).expect("extra dir");
    let output_file = extra.join("generated.txt");
    let prompt = format!(
        "request_permissions_then_bash {} :: printf granted > {}",
        extra.display(),
        output_file.display()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--cwd",
            workspace.to_str().unwrap(),
            &prompt,
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&output_file).unwrap(), "granted");

    let events = parse_jsonl(&output.stdout);
    let requested = events
        .iter()
        .filter(|event| event["type"] == "tool.call.requested")
        .map(|event| event["payload"]["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requested, vec!["request_permissions", "bash"]);
    let completed = events
        .iter()
        .filter(|event| event["type"] == "tool.call.completed")
        .collect::<Vec<_>>();
    assert_eq!(completed[0]["payload"]["status"], "completed");
    assert_eq!(completed[1]["payload"]["status"], "completed");
}

fn sandbox_seatbelt_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        let available = Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        assert!(
            available,
            "macOS Seatbelt is required for sandbox contract tests"
        );
        available
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[test]
fn auto_edit_allows_edit_tool() {
    let _guard = tool_cli_test_guard();
    let temp_dir = make_temp_workspace("edit-success");
    let file_path = temp_dir.join("note.txt");
    fs::write(&file_path, "hello orca\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "auto-edit",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "edit note.txt :: hello => hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "hi orca\n");

    let events = parse_jsonl(&output.stdout);
    let resolved = find_event(&events, "approval.resolved");
    assert_eq!(resolved["payload"]["decision"], "allow");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "edit");
    assert_eq!(completed["payload"]["status"], "completed");
}

#[test]
fn suggest_denies_edit_in_jsonl_mode() {
    let _guard = tool_cli_test_guard();
    let temp_dir = make_temp_workspace("edit-denied");
    let file_path = temp_dir.join("note.txt");
    fs::write(&file_path, "hello orca\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "suggest",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "edit note.txt :: hello => hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello orca\n");

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "edit");
    assert_eq!(completed["payload"]["status"], "denied");
}

#[test]
fn update_plan_emits_plan_updated_event() {
    let _guard = tool_cli_test_guard();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "plan implementing todo support",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "update_plan");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "3 items");

    let plan = find_event(&events, "plan.updated");
    assert_eq!(plan["payload"]["explanation"], "implementing todo support");
    assert_eq!(plan["payload"]["plan"][0]["step"], "Inspect references");
    assert_eq!(plan["payload"]["plan"][0]["status"], "completed");
    assert_eq!(
        plan["payload"]["plan"][1]["step"],
        "Implement task plan support"
    );
    assert_eq!(plan["payload"]["plan"][1]["status"], "in_progress");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "update_plan");
    assert_eq!(completed["payload"]["status"], "completed");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Plan updated")
    );
}

#[test]
fn malformed_update_plan_is_schema_failed_and_can_be_retried() {
    let _guard = tool_cli_test_guard();
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "bad_plan_then_fix",
        ])
        .output()
        .expect("run orca");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let failed_plan = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.completed"
                && event["payload"]["name"] == "update_plan"
                && event["payload"]["status"] == "failed"
        })
        .expect("malformed update_plan should fail before execution");
    assert!(
        failed_plan["payload"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("tool arguments failed schema validation"),
        "error={failed_plan:?}"
    );
    let plan_updates = events
        .iter()
        .filter(|event| event["type"] == "plan.updated")
        .count();
    assert_eq!(plan_updates, 1, "events={events:?}");
    assert_eq!(events.last().unwrap()["type"], "session.completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn external_tool_descriptor_runs_from_orca_tools_dir() {
    let _guard = tool_cli_test_guard();
    let home = make_temp_workspace("external-home");
    let tools_dir = home.join("tools");
    fs::create_dir_all(&tools_dir).expect("tools dir");
    let workspace = make_temp_workspace("external-workspace");
    fs::create_dir_all(workspace.join("scripts")).expect("scripts dir");
    let output_file = workspace.join("deploy-output.txt");
    #[cfg(unix)]
    let command = {
        let script = workspace.join("scripts/deploy.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > {}\nprintf 'deploy ok'\n",
                shell_escape(&output_file)
            ),
        )
        .expect("write script");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        "./scripts/deploy.sh".to_string()
    };
    #[cfg(windows)]
    let command = {
        // Keep this test native on Windows: external tools are evaluated by
        // the resolved PowerShell/cmd dialect rather than a Unix fixture.
        let output_path = output_file
            .to_string_lossy()
            .replace('`', "``")
            .replace('"', "`\"");
        format!(
            "$input | Set-Content -LiteralPath \"{output_path}\" -NoNewline; Write-Host -NoNewline \"deploy ok\""
        )
    };
    fs::write(
        tools_dir.join("deploy.toml"),
        toml::to_string(&ExternalToolConfig {
            name: "deploy".to_string(),
            description: "Deploy the current branch".to_string(),
            action_kind: ActionKind::Write,
            command,
            schema: serde_json::json!({
                "target": { "type": "string", "description": "environment" }
            }),
        })
        .expect("serialize descriptor"),
    )
    .expect("write descriptor");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--cwd",
            workspace.to_str().unwrap(),
            r#"external deploy {"target":"staging"}"#,
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&output_file).expect("external tool stdin"),
        r#"{"target":"staging"}"#
    );

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "deploy");
    assert_eq!(requested["payload"]["action"], "write");
    assert_eq!(requested["payload"]["target"], "deploy");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "deploy");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "deploy ok");
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type}"))
}

fn tool_cli_test_guard() -> MutexGuard<'static, ()> {
    // Recover from a poisoned lock so a single failing test is reported on its
    // own merits instead of cascading into every other test failing with
    // `PoisonError`. The guard only serializes access; there is no shared state
    // left inconsistent by a panicking test.
    TOOL_CLI_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn make_temp_workspace(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("orca-{name}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp workspace");
    dir
}

fn find_task_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if !root.exists() {
        return files;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read task dir") {
            let path = entry.expect("task dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some("tasks.json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[cfg(unix)]
fn shell_escape(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
