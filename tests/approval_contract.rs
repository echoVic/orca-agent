use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn suggest_denies_write_in_jsonl_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "suggest",
            "write a file",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));

    let events = parse_jsonl(&output.stdout);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "approval.requested")
    );
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved" && event["payload"]["decision"] == "deny"
    }));
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "approval_required"
    );
}

#[test]
fn auto_edit_allows_sandboxed_shell_in_jsonl_mode() {
    if !matches!(
        orca_tools::sandbox::enforcement_state(),
        orca_core::capability::EnforcementState::Enforced
    ) {
        return;
    }
    let home = TempDir::new().expect("temp home");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "auto-edit",
            "bash echo hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved"
            && event["payload"]["decision"] == "allow"
            && event["payload"]["reason"] == "auto-edit permits shell"
    }));
    assert_allowed_shell_terminal(&events);
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn permission_allow_rule_allows_matching_shell_in_jsonl_mode() {
    if !matches!(
        orca_tools::sandbox::enforcement_state(),
        orca_core::capability::EnforcementState::Enforced
    ) {
        return;
    }
    let home = TempDir::new().expect("temp home");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[[permissions.rules]]
tool = "bash"
pattern = "echo *"
decision = "allow"
"#,
    )
    .expect("write config");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        &std::env::current_dir().expect("cwd"),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust approval contract workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "bash echo hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved" && event["payload"]["decision"] == "allow"
    }));
    assert_allowed_shell_terminal(&events);
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn permission_deny_rule_overrides_matching_allow_rule() {
    let home = TempDir::new().expect("temp home");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[[permissions.rules]]
tool = "bash"
pattern = "echo *"
decision = "allow"

[[permissions.rules]]
tool = "bash"
pattern = "echo secret*"
decision = "deny"
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "bash echo secret-token",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved" && event["payload"]["decision"] == "deny"
    }));
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "approval_required"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn assert_allowed_shell_terminal(events: &[Value]) {
    let terminal = events
        .iter()
        .find(|event| event["type"] == "tool.call.completed")
        .unwrap_or_else(|| panic!("missing shell terminal event: {events:#?}"));

    #[cfg(windows)]
    {
        assert_eq!(terminal["payload"]["status"], "failed", "{events:#?}");
        let terminal_text = terminal.to_string();
        assert!(
            terminal_text.contains("Windows sandbox") && terminal_text.contains("setup"),
            "Windows shell without a provisioned capability must fail closed: {events:#?}"
        );
    }

    #[cfg(not(windows))]
    assert_eq!(terminal["payload"]["status"], "completed", "{events:#?}");
}
