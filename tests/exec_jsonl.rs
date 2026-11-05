use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

fn platform_history_truncation_command() -> &'static str {
    #[cfg(windows)]
    {
        r#"bash Get-ChildItem -LiteralPath $env:ORCA_HOME -Recurse -Filter '*.jsonl' -File | ForEach-Object { $file = [System.IO.File]::Open($_.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite); try { $file.SetLength(0); if ($file.Length -ne 0) { throw 'history truncation failed' }; [System.IO.File]::WriteAllText((Join-Path (Get-Location) 'truncation-observed.txt'), 'truncated') } finally { $file.Dispose() } }"#
    }
    #[cfg(not(windows))]
    {
        r#"bash find "$ORCA_HOME/sessions" -name '*.jsonl' -type f -exec sh -c ': > "$1"; test ! -s "$1" && printf "truncated" > truncation-observed.txt' sh {} \;"#
    }
}

fn platform_shell_write_command() -> &'static str {
    #[cfg(windows)]
    {
        "bash [System.IO.File]::WriteAllText((Join-Path (Get-Location) 'note.txt'), 'shell content')"
    }
    #[cfg(not(windows))]
    {
        "bash printf 'shell content' > note.txt"
    }
}

fn platform_configure_fixture_shell(command: &mut Command) {
    #[cfg(windows)]
    command.env("ORCA_SHELL", "powershell.exe");
    #[cfg(not(windows))]
    command.env("ORCA_SHELL", "/bin/sh");
}

fn assert_observed_workspace_change(observation: &Value, path: &str) {
    assert_eq!(observation["attribution"], "unknown");
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    {
        assert_eq!(observation["changed"], true);
        assert!(
            observation["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == path)
        );
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        let _ = path;
        assert!(observation["changed"].is_null());
        assert_eq!(observation["complete"], false);
        assert!(
            observation["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "safe_directory_traversal_unavailable")
        );
    }
}

#[test]
fn exec_truncated_history_never_advertises_recoverability() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    platform_configure_fixture_shell(&mut command);
    let output = command
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--json",
            "--provider",
            "mock",
            "--yolo",
            "--save-history",
            platform_history_truncation_command(),
        ])
        .output()
        .unwrap();
    let events = parse_jsonl(&output.stdout);
    let payload = &find_event(&events, "session.completed")["payload"];
    assert_eq!(payload["resumable"], false, "{output:?}");
    assert_eq!(payload["resume_unavailable_reason"], "history_write_failed");
    assert!(payload["resume"].is_null());
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("truncation-observed.txt")).unwrap(),
        "truncated",
        "fixture shell must confirm zero-length history before later records append"
    );
}

#[test]
fn exec_completion_exposes_actual_recovery_and_workspace_observation() {
    for save in [false, true] {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        std::fs::write(cwd.path().join("note.txt"), "before").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command
            .env("ORCA_HOME", home.path())
            .current_dir(cwd.path())
            .args(["exec", "--json", "--provider", "mock", "--yolo"]);
        if save {
            command.arg("--save-history");
        } else {
            command.arg("--no-history");
        }
        let output = command
            .arg("edit note.txt :: before => after")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let events = parse_jsonl(&output.stdout);
        let payload = &find_event(&events, "session.completed")["payload"];
        assert_eq!(payload["resumable"], save);
        if save {
            assert!(payload["resume"]["session_id"].as_str().is_some());
            assert_eq!(payload["resume"]["command"], "orca exec resume");
        } else {
            assert_eq!(payload["resume_unavailable_reason"], "history_disabled");
            assert!(payload["resume"].is_null());
        }
        assert_observed_workspace_change(&payload["workspace_observation"], "note.txt");
    }
}

#[test]
fn exec_observes_shell_write_from_preexisting_dirty_content_without_write_receipt() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    std::fs::write(cwd.path().join("note.txt"), "preexisting dirty content").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    platform_configure_fixture_shell(&mut command);
    let output = command
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--json",
            "--provider",
            "mock",
            "--yolo",
            "--no-history",
            platform_shell_write_command(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = parse_jsonl(&output.stdout);
    let payload = &find_event(&events, "session.completed")["payload"];
    assert_eq!(payload["writes_applied"], false);
    assert_observed_workspace_change(&payload["workspace_observation"], "note.txt");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("note.txt")).unwrap(),
        "shell content"
    );
}

#[test]
fn exec_session_completed_reports_writes_applied_false_without_writes() {
    let workspace = TempDir::new().expect("temporary workspace");
    let home = TempDir::new().expect("temporary ORCA_HOME");
    std::fs::write(workspace.path().join("note.txt"), "before").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(workspace.path())
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--no-history",
            "read note.txt",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = parse_jsonl(&output.stdout);
    assert_eq!(
        find_event(&events, "session.completed")["payload"]["writes_applied"],
        false
    );
}

#[test]
fn exec_session_completed_reports_writes_applied_true_after_a_write() {
    let workspace = TempDir::new().expect("temporary workspace");
    let home = TempDir::new().expect("temporary ORCA_HOME");
    std::fs::write(workspace.path().join("note.txt"), "before").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(workspace.path())
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--no-history",
            "--mode",
            "full-auto",
            "edit note.txt :: before => after",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
        "after"
    );
    let events = parse_jsonl(&output.stdout);
    assert_eq!(
        find_event(&events, "session.completed")["payload"]["writes_applied"],
        true
    );
}

#[test]
fn exec_session_completed_includes_subagent_writes() {
    for mode in ["sync", "async", "sync subagent sync"] {
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "before").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .current_dir(workspace.path())
            .env("ORCA_HOME", home.path())
            .args([
                "exec",
                "--output-format",
                "jsonl",
                "--provider",
                "mock",
                "--no-history",
                "--mode",
                "full-auto",
                &format!("subagent {mode} edit note.txt :: before => after"),
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{mode}: {output:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "after"
        );
        let events = parse_jsonl(&output.stdout);
        assert_eq!(
            find_event(&events, "session.completed")["payload"]["writes_applied"],
            true,
            "{mode}"
        );
    }
}

#[test]
fn exec_session_completed_does_not_count_readonly_or_failed_subagent_writes() {
    for mode in ["sync", "async"] {
        for prompt in ["read note.txt", "edit note.txt :: missing => after"] {
            let workspace = TempDir::new().unwrap();
            let home = TempDir::new().unwrap();
            std::fs::write(workspace.path().join("note.txt"), "before").unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_orca"))
                .current_dir(workspace.path())
                .env("ORCA_HOME", home.path())
                .args([
                    "exec",
                    "--output-format",
                    "jsonl",
                    "--provider",
                    "mock",
                    "--no-history",
                    "--mode",
                    "full-auto",
                    &format!("subagent {mode} {prompt}"),
                ])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(0), "{mode} {prompt}: {output:?}");
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
                "before"
            );
            let events = parse_jsonl(&output.stdout);
            assert_eq!(
                find_event(&events, "session.completed")["payload"]["writes_applied"],
                false,
                "{mode} {prompt}"
            );
        }
    }
}

#[test]
fn exec_outputs_jsonl_contract_and_success_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 5);
    assert_eq!(events[0]["version"], "1");
    assert_eq!(events[0]["type"], "session.started");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "assistant.reasoning.delta")
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "assistant.message.delta")
    );
    assert_eq!(events.last().unwrap()["type"], "session.completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");

    assert!(
        events
            .windows(2)
            .all(|pair| pair[0]["seq"].as_u64() < pair[1]["seq"].as_u64())
    );
    for event in &events {
        let run_id = event["run_id"].as_str().expect("run id string");
        assert_eq!(
            uuid::Uuid::parse_str(run_id)
                .expect("UUID run id")
                .get_version_num(),
            7
        );
    }
}

#[test]
fn exec_reads_prompt_from_piped_stdin_when_prompt_is_omitted() {
    let output = run_exec_with_stdin(
        &["exec", "--output-format", "jsonl", "--provider", "mock"],
        "prompt from stdin\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let turn_started = find_event(&events, "turn.started");
    // The prompt reaches the model byte-exact, including the newline that came
    // from stdin (issue #66).
    assert_eq!(turn_started["payload"]["prompt"], "prompt from stdin\n");
}

#[test]
fn exec_dash_prompt_reads_piped_stdin_as_prompt() {
    let output = run_exec_with_stdin(
        &[
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "-",
        ],
        "dash prompt from stdin\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let turn_started = find_event(&events, "turn.started");
    assert_eq!(
        turn_started["payload"]["prompt"],
        "dash prompt from stdin\n"
    );
}

#[test]
fn exec_appends_piped_stdin_to_prompt_argument() {
    let output = run_exec_with_stdin(
        &[
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "summarize this",
        ],
        "extra context\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let turn_started = find_event(&events, "turn.started");
    assert_eq!(
        turn_started["payload"]["prompt"],
        "summarize this\n\n<stdin>\nextra context\n</stdin>"
    );
}

#[test]
fn exec_without_prompt_rejects_empty_piped_stdin() {
    let output = run_exec_with_stdin(
        &["exec", "--output-format", "jsonl", "--provider", "mock"],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No prompt provided via stdin."));
    assert!(output.stdout.is_empty());
}

#[test]
fn exec_emits_usage_event_when_provider_reports_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let usage = events
        .iter()
        .find(|event| event["type"] == "usage.updated")
        .expect("usage event");
    assert_eq!(usage["payload"]["input_tokens"], 120);
    assert_eq!(usage["payload"]["output_tokens"], 30);
    assert_eq!(usage["payload"]["cache_tokens"], 10);
    assert_eq!(usage["payload"]["total_tokens"], 150);
    assert!(usage["payload"]["estimated_cost_usd"].as_f64().unwrap() > 0.0);
    let observation = &find_event(&events, "model.response.completed")["payload"]["observation"];
    let elapsed = observation["elapsed_ms"]
        .as_u64()
        .expect("provider elapsed time");
    let first = observation["first_delta_ms"]
        .as_u64()
        .expect("first streamed delta");
    assert!(first <= elapsed, "{observation}");
    assert!(matches!(
        observation["first_delta_kind"].as_str(),
        Some("reasoning" | "text" | "tool")
    ));
}

#[test]
fn exec_pre_model_hook_injects_context_into_model_input() {
    let home = TempDir::new().expect("temp home");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[[hooks]]
event = "pre_model_call"
command = "printf '%s' '{\"action\":\"inject\",\"context\":\"hooked policy context\"}'"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_system_echo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "assistant.message.delta"
            && event["payload"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("hooked policy context")
    }));
}

#[test]
fn exec_pre_model_hook_rejects_unknown_structured_action() {
    let home = TempDir::new().expect("temp home");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[[hooks]]
event = "pre_model_call"
command = "printf '%s' '{\"action\":\"injcet\",\"context\":\"typo should fail\"}'"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["payload"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("unsupported hook action 'injcet'")
    }));
    assert_eq!(events.last().unwrap()["type"], "session.completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

#[test]
fn exec_post_model_hook_observes_usage_environment() {
    let home = TempDir::new().expect("temp home");
    let usage_path = home.path().join("usage.txt");
    #[cfg(unix)]
    let hook_command = format!(
        "printf '%s %s %s' \"$ORCA_USAGE_INPUT_TOKENS\" \"$ORCA_USAGE_OUTPUT_TOKENS\" \"$ORCA_USAGE_CACHE_TOKENS\" > {}",
        shell_escape(&usage_path)
    );
    #[cfg(windows)]
    let hook_command = format!(
        "Set-Content -LiteralPath '{}' -Value \"$env:ORCA_USAGE_INPUT_TOKENS $env:ORCA_USAGE_OUTPUT_TOKENS $env:ORCA_USAGE_CACHE_TOKENS\" -NoNewline",
        usage_path.display().to_string().replace('\'', "''")
    );
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
update_check = false
auto_memory = false
mode = "full-auto"

[[hooks]]
event = "post_model_call"
command = {}
"#,
            toml::Value::String(hook_command)
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read_to_string(&usage_path).expect("usage hook output"),
        "120 30 10"
    );
}

#[test]
fn exec_auto_model_defaults_to_flash() {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let routed = events
        .iter()
        .find(|event| event["type"] == "model.routed")
        .expect("model routed event");
    assert_eq!(routed["payload"]["requested_model"], "auto");
    assert_eq!(routed["payload"]["actual_model"], "deepseek-flash");
    assert_eq!(routed["payload"]["reason"], "default_flash");
}

#[test]
fn exec_auto_model_routes_any_prompt_to_flash() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--model",
            "auto",
            "review this migration plan",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let routed = events
        .iter()
        .find(|event| event["type"] == "model.routed")
        .expect("model routed event");
    assert_eq!(routed["payload"]["requested_model"], "auto");
    assert_eq!(routed["payload"]["actual_model"], "deepseek-flash");
    assert_eq!(routed["payload"]["reason"], "default_flash");
}

#[test]
fn exec_retired_flash_alias_routes_as_canonical_flash() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--model",
            "deepseek-v4-flash",
            "review this migration plan",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let routed = events
        .iter()
        .find(|event| event["type"] == "model.routed")
        .expect("model routed event");
    assert_eq!(routed["payload"]["requested_model"], "deepseek-flash");
    assert_eq!(routed["payload"]["actual_model"], "deepseek-flash");
    assert_eq!(routed["payload"]["reason"], "explicit");
}

#[test]
fn exec_config_layers_respect_project_env_and_cli_precedence() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::create_dir_all(project.path().join(".orca")).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        r#"
model = "deepseek-flash"
mode = "suggest"
"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join(".orca/config.toml"),
        r#"
model = "deepseek-v4-pro"
mode = "auto-edit"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .env("ORCA_MODEL", "deepseek-flash")
        .env("ORCA_MODE", "full-auto")
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            project.path().to_str().unwrap(),
            "--model",
            "auto",
            "--mode",
            "plan",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events[0]["payload"]["approval_mode"], "plan");
    let routed = events
        .iter()
        .find(|event| event["type"] == "model.routed")
        .expect("model routed event");
    assert_eq!(routed["payload"]["requested_model"], "auto");
}

#[test]
fn exec_stops_when_usage_exceeds_max_budget() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--max-budget",
            "0.000001",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(4));

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["payload"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("budget")
    }));
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "budget_exhausted"
    );
}

#[test]
fn session_completed_carries_durable_session_id_when_history_is_recorded() {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--save-history",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "session.completed");
    let session_id = terminal["payload"]["session_id"]
        .as_str()
        .expect("durable session id in terminal event");
    assert!(!session_id.is_empty());

    let documents = session_documents(home.path());
    assert_eq!(documents.len(), 1);
    let meta = documents[0]
        .1
        .iter()
        .find(|record| record["type"] == "session.meta")
        .expect("session metadata");
    assert_eq!(meta["session_id"].as_str(), Some(session_id));
}

#[test]
fn session_completed_omits_session_id_when_history_is_disabled() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "session.completed");
    assert!(terminal["payload"]["session_id"].is_null());
}

#[test]
fn budget_exhausted_text_mode_prints_resume_hint_with_session_id() {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--max-budget",
            "0.000001",
            "mock_usage",
        ])
        .output()
        .expect("run budget-limited fixture");

    assert_eq!(output.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let documents = session_documents(home.path());
    assert_eq!(documents.len(), 1);
    let session_id = documents[0]
        .1
        .iter()
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("session metadata")
        .to_string();
    assert!(
        stdout.contains(&format!(
            "To continue this session, run: orca exec resume {session_id}"
        )),
        "headless exit must surface the exact resume command"
    );
}

#[test]
fn successful_text_mode_does_not_print_resume_hint() {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "mock_usage"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("To continue this session, run:"),
        "successful runs do not print a resume hint"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn session_documents(home: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<Value>)> {
    fn collect(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, files);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect(&home.join("sessions"), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let records = std::fs::read_to_string(&path)
                .expect("read saved conversation")
                .lines()
                .map(|line| serde_json::from_str(line).expect("valid saved conversation record"))
                .collect();
            (path, records)
        })
        .collect()
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type}"))
}

fn run_exec_with_stdin(args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca");

    {
        let child_stdin = child.stdin.as_mut().expect("child stdin");
        child_stdin
            .write_all(stdin.as_bytes())
            .expect("write child stdin");
    }

    child.wait_with_output().expect("wait for orca")
}

#[cfg(unix)]
fn shell_escape(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[test]
fn exec_blocked_jsonl_resume_uses_advertised_durable_session() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--json",
            "--save-history",
            "--provider",
            "mock",
            "report_blocker user_decision need a port ? Use port 8080?",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let events = parse_jsonl(&output.stdout);
    let payload = &find_event(&events, "session.completed")["payload"];
    assert_eq!(payload["resumable"], true);
    let session_id = payload["resume"]["session_id"].as_str().unwrap();
    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "resume",
            session_id,
            "--json",
            "--provider",
            "mock",
            "Yes, use port 8080",
        ])
        .output()
        .unwrap();
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    let events = parse_jsonl(&resumed.stdout);
    let payload = &find_event(&events, "session.completed")["payload"];
    assert_eq!(payload["status"], "success");
    assert_eq!(payload["resume"]["session_id"], session_id);
    assert!(payload["blockers"].as_array().unwrap().is_empty());
    let documents = session_documents(home.path());
    assert!(
        documents
            .iter()
            .flat_map(|(_, records)| records)
            .any(|record| record.to_string().contains("Yes, use port 8080"))
    );
}

#[test]
fn exec_report_blocker_ends_blocked_with_exit_5() {
    assert_exec_report_blocker(false);
}

#[test]
fn exec_report_blocker_with_passing_verifier_stays_blocked() {
    assert_exec_report_blocker(true);
}

#[test]
fn exec_report_blocker_carries_question() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--no-history",
            "--provider",
            "mock",
            "report_blocker user_decision need a port ? Use port 8080?",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let terminal = &events.last().unwrap()["payload"]["terminal"]["blocked"]["blocker"];
    assert_eq!(terminal["summary"], "need a port");
    assert_eq!(terminal["question"], "Use port 8080?");
}

#[test]
fn exec_blocked_text_mode_prints_question_without_resume_line_when_history_is_off() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "text",
            "--no-history",
            "--provider",
            "mock",
            "report_blocker user_decision need a port ? Use port 8080?",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("blocked: user_decision — need a port\nQuestion: Use port 8080?"),
        "{stdout}"
    );
    assert!(!stdout.contains("orca exec resume"), "{stdout}");
}

#[test]
fn exec_blocked_text_mode_prints_answer_resume_hint_with_saved_session_id() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "text",
            "--save-history",
            "--provider",
            "mock",
            "report_blocker user_decision need a port ? Use port 8080?",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let documents = session_documents(home.path());
    assert_eq!(documents.len(), 1);
    let session_id = documents[0]
        .1
        .iter()
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("saved session id");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("blocked: user_decision — need a port\nQuestion: Use port 8080?"),
        "{stdout}"
    );
    let hint = stdout
        .lines()
        .find(|line| line.starts_with("To continue this session, run:"))
        .expect("resume hint");
    assert_eq!(
        hint,
        format!("To continue this session, run: orca exec resume {session_id} \"<your answer>\"")
    );
}

#[test]
fn exec_blocked_text_mode_without_question_keeps_the_plain_resume_hint() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "text",
            "--save-history",
            "--provider",
            "mock",
            "report_blocker user_decision need a port",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let documents = session_documents(home.path());
    assert_eq!(documents.len(), 1);
    let session_id = documents[0]
        .1
        .iter()
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("saved session id");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("blocked: user_decision — need a port"),
        "{stdout}"
    );
    let hint = stdout
        .lines()
        .find(|line| line.starts_with("To continue this session, run:"))
        .expect("resume hint");
    assert_eq!(
        hint,
        format!("To continue this session, run: orca exec resume {session_id}")
    );
    assert!(!stdout.contains("<your answer>"), "{stdout}");
}

#[test]
fn exec_report_blocker_text_mode_prints_the_reason() {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let cwd = TempDir::new().expect("temporary workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "text",
            "--provider",
            "mock",
            "--no-history",
            "report_blocker environment_contradiction workspace is read-only",
        ])
        .output()
        .expect("run orca");
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status: blocked"), "{stdout}");
    assert!(
        stdout.contains("blocked: environment_contradiction — workspace is read-only"),
        "{stdout}"
    );
}

fn assert_exec_report_blocker(with_verifier: bool) {
    let home = TempDir::new().expect("temporary ORCA_HOME");
    let cwd = TempDir::new().expect("temporary workspace");
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    command
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--no-history",
        ]);
    if with_verifier {
        command.args(["--verifier", "printf ok"]);
    }
    let output = command
        .arg("report_blocker environment_contradiction workspace is read-only")
        .output()
        .expect("run orca");
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let terminal = events.last().expect("terminal event");
    assert_eq!(terminal["type"], "session.completed");
    assert_eq!(terminal["payload"]["status"], "blocked");
    let usage = &terminal["payload"]["terminal"]["blocked"]["usage"];
    assert!(usage["turns"].as_u64().expect("turn count") >= 1);
    assert!(usage["tool_calls"].as_u64().expect("tool call count") >= 1);
    let blocker = &terminal["payload"]["terminal"]["blocked"]["blocker"];
    assert_eq!(blocker["kind"], "environment_contradiction");
    assert_eq!(blocker["summary"], "workspace is read-only");
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "verification.started")
    );
}

#[test]
fn exec_subagent_report_blocker_reports_blocked_to_parent() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--no-history",
            "subagent sync report_blocker",
        ])
        .output()
        .expect("run orca");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "session.completed");
    assert_eq!(terminal["payload"]["status"], "success");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "session.completed")
            .count(),
        1
    );
    let tasks = events
        .iter()
        .rev()
        .find(|e| e["type"] == "workflow.tasks.updated")
        .expect("task snapshot")["payload"]["tasks"]
        .as_array()
        .unwrap();
    let child = tasks
        .iter()
        .find(|task| task["type"] == "subagent")
        .expect("child task");
    assert_eq!(child["status"], "blocked", "{tasks:?}");
    let blockers = terminal["payload"]["blockers"]
        .as_array()
        .expect("aggregated blockers");
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0]["task_id"], child["id"]);
    assert_eq!(blockers[0]["kind"], "environment_contradiction");
    assert_eq!(blockers[0]["summary"], "child cannot write");
    assert_eq!(blockers[0]["resolution"], "unresolved");
    let parent = tasks
        .iter()
        .find(|task| task["type"] == "main_session")
        .expect("root task");
    assert_eq!(parent["status"], "completed");
    let listed = events
        .iter()
        .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "task_list")
        .expect("parent lists tasks after child blocks");
    let listed: Value =
        serde_json::from_str(listed["payload"]["output"].as_str().unwrap()).unwrap();
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == child["id"] && task["status"] == "blocked"),
        "{listed}"
    );
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == parent["id"] && task["status"] == "running"),
        "{listed}"
    );
    let completed = events
        .iter()
        .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "subagent")
        .expect("subagent tool result");
    assert_eq!(completed["payload"]["status"], "failed");
    let output = completed["payload"]["output"]
        .as_str()
        .expect("blocked output");
    for line in [
        "Subagent status: Blocked",
        "kind: environment_contradiction",
        "summary: child cannot write",
    ] {
        assert!(output.contains(line), "{output}");
    }
}

#[test]
fn exec_subagent_report_blocker_carries_its_question_to_parent() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--no-history",
            "subagent sync report_blocker ? Can you allow writes?",
        ])
        .output()
        .expect("run orca");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "session.completed");
    assert_eq!(terminal["payload"]["status"], "success");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "session.completed")
            .count(),
        1
    );
    let tasks = events
        .iter()
        .rev()
        .find(|e| e["type"] == "workflow.tasks.updated")
        .expect("task snapshot")["payload"]["tasks"]
        .as_array()
        .unwrap();
    let child = tasks
        .iter()
        .find(|task| task["type"] == "subagent")
        .expect("child task");
    assert_eq!(child["status"], "blocked", "{tasks:?}");
    let parent = tasks
        .iter()
        .find(|task| task["type"] == "main_session")
        .expect("root task");
    assert_eq!(parent["status"], "completed");
    let listed = events
        .iter()
        .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "task_list")
        .expect("parent lists tasks after child blocks");
    let listed: Value =
        serde_json::from_str(listed["payload"]["output"].as_str().unwrap()).unwrap();
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == child["id"] && task["status"] == "blocked"),
        "{listed}"
    );
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == parent["id"] && task["status"] == "running"),
        "{listed}"
    );
    let completed = events
        .iter()
        .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "subagent")
        .expect("subagent tool result");
    assert_eq!(completed["payload"]["status"], "failed");
    let output = completed["payload"]["output"]
        .as_str()
        .expect("blocked output");
    for line in [
        "Subagent status: Blocked",
        "kind: environment_contradiction",
        "summary: child cannot write",
        "question: Can you allow writes?",
    ] {
        assert!(output.contains(line), "{output}");
    }
}

fn blocker_task_view(home: &std::path::Path, cwd: &std::path::Path, task_id: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .current_dir(cwd)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--save-history",
            &format!("task_read_output {task_id}"),
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "task_read_output")
        .unwrap();
    serde_json::from_str(completed["payload"]["output"].as_str().unwrap()).unwrap()
}

#[test]
fn exec_async_subagent_report_blocker_persists_blocked() {
    for question in [None, Some("Can the workspace be made writable?")] {
        let prompt = question.map_or_else(
            || "subagent async report_blocker".to_string(),
            |question| format!("subagent async report_blocker ? {question}"),
        );
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("ORCA_HOME", home.path())
            .current_dir(cwd.path())
            .args([
                "exec",
                "--output-format",
                "jsonl",
                "--provider",
                "mock",
                "--approval-mode",
                "full-auto",
                "--save-history",
                &prompt,
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = parse_jsonl(&output.stdout);
        let launch = events
            .iter()
            .find(|e| e["type"] == "tool.call.completed" && e["payload"]["name"] == "subagent")
            .unwrap();
        let launch: Value =
            serde_json::from_str(launch["payload"]["output"].as_str().unwrap()).unwrap();
        let task_id = launch["agent_id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let child = blocker_task_view(home.path(), cwd.path(), task_id);
            if child["status"] == "blocked" {
                let result = child["error"]
                    .as_str()
                    .or_else(|| child["result"].as_str())
                    .expect("blocked child result");
                for expected in [
                    "Subagent status: Blocked",
                    "kind: environment_contradiction",
                    "summary: child cannot write",
                ] {
                    assert!(result.contains(expected), "{child}");
                }
                assert!(!result.contains("BlockerSummary {"), "{child}");
                if let Some(question) = question {
                    assert!(result.contains(&format!("question: {question}")), "{child}");
                }
                break;
            }
            assert_ne!(child["status"], "failed", "{child}");
            assert!(
                std::time::Instant::now() < deadline,
                "child did not block: {child}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[test]
fn root_exec_mode_json_and_common_options_reach_the_runtime() {
    for mode in ["plan", "full-auto"] {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        std::fs::write(cwd.path().join("note.txt"), "before").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("ORCA_HOME", home.path())
            .env("ORCA_MODE", "suggest")
            .args([
                "--mode",
                mode,
                "-m",
                "deepseek-v4-pro",
                "-C",
                cwd.path().to_str().unwrap(),
                "--provider",
                "mock",
                "exec",
                "--json",
                "--no-history",
                "edit note.txt :: before => after",
            ])
            .output()
            .unwrap();
        let expected_status = if mode == "plan" { 3 } else { 0 };
        assert_eq!(output.status.code(), Some(expected_status), "{output:?}");
        let events = parse_jsonl(&output.stdout);
        assert_eq!(events[0]["payload"]["approval_mode"], mode);
        assert_eq!(
            find_event(&events, "model.routed")["payload"]["requested_model"],
            "deepseek-v4-pro"
        );
        let expected = if mode == "full-auto" {
            "after"
        } else {
            "before"
        };
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("note.txt")).unwrap(),
            expected
        );
    }
}

#[test]
fn exec_last_multiword_prompt_resumes_the_saved_session_with_an_explicit_mode() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let initial = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "exec",
            "--json",
            "--save-history",
            "--provider",
            "mock",
            "--mode",
            "suggest",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(initial.status.code(), Some(0), "{initial:?}");
    let initial_events = parse_jsonl(&initial.stdout);
    let session = find_event(&initial_events, "session.completed")["payload"]["session_id"]
        .as_str()
        .unwrap();
    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(cwd.path())
        .args([
            "--plan",
            "exec",
            "resume",
            "--last",
            "--json",
            "--provider",
            "mock",
            "fix",
            "failing",
            "test",
        ])
        .output()
        .unwrap();
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    let events = parse_jsonl(&resumed.stdout);
    assert_eq!(events[0]["payload"]["approval_mode"], "plan");
    assert_eq!(
        find_event(&events, "session.completed")["payload"]["session_id"],
        session
    );
    let documents = session_documents(home.path());
    assert_eq!(documents.len(), 1);
    assert!(
        documents[0]
            .1
            .iter()
            .any(|record| record["type"] == "conversation.message"
                && record["message"]["content"] == "fix failing test"),
        "saved prompt: {:?}",
        documents[0].1
    );
}
