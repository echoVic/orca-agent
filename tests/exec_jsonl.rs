use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

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

    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], seq);
        assert!(event["run_id"].as_str().unwrap().starts_with("run-"));
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
    assert_eq!(turn_started["payload"]["prompt"], "prompt from stdin");
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
    assert_eq!(turn_started["payload"]["prompt"], "dash prompt from stdin");
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
fn exec_auto_model_defaults_to_pro() {
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

    let events = parse_jsonl(&output.stdout);
    let routed = events
        .iter()
        .find(|event| event["type"] == "model.routed")
        .expect("model routed event");
    assert_eq!(routed["payload"]["requested_model"], Value::Null);
    assert_eq!(routed["payload"]["actual_model"], "deepseek-v4-pro");
    assert_eq!(routed["payload"]["reason"], "default_pro");
}

#[test]
fn exec_auto_model_routes_any_prompt_to_pro() {
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
    assert_eq!(routed["payload"]["actual_model"], "deepseek-v4-pro");
    assert_eq!(routed["payload"]["reason"], "default_pro");
}

#[test]
fn exec_explicit_model_disables_auto_route() {
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
    assert_eq!(routed["payload"]["requested_model"], "deepseek-v4-flash");
    assert_eq!(routed["payload"]["actual_model"], "deepseek-v4-flash");
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
model = "deepseek-v4-flash"
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
        .env("ORCA_MODEL", "deepseek-v4-flash")
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
        .args(["exec", "--provider", "mock", "mock_repeat_read 256"])
        .output()
        .expect("run max-turn fixture");

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
