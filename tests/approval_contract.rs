use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

#[path = "support/restricted_shell.rs"]
mod restricted_shell;
use restricted_shell::host_refuses_restricted_shells;

#[test]
fn yolo_runs_a_host_shell_without_approval_despite_suggest_config() {
    let home = TempDir::new().expect("temp home");
    let cwd = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"suggest\"\n")
        .expect("suggest config");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust workspace");
    let marker = outside.path().join("yolo.txt");
    let prompt = format!("bash echo YOLO_HOST_ACCESS > \"{}\"", marker.display());
    for args in [vec!["exec", "--yolo"], vec!["--yolo", "exec"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("ORCA_HOME", home.path())
            .env("ORCA_MODE", "suggest")
            .env_remove("ORCA_SHELL")
            .args(args)
            .args(["--output-format", "jsonl", "--provider", "mock", "--cwd"])
            .arg(cwd.path())
            .arg(&prompt)
            .output()
            .expect("run yolo");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events = parse_jsonl(&output.stdout);
        assert_full_auto_shell(&events);
        assert_eq!(events.last().unwrap()["payload"]["status"], "success");
        assert_eq!(read_shell_marker(&marker).trim(), "YOLO_HOST_ACCESS");
        std::fs::remove_file(&marker).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "mode = \"suggest\"\n"
    );
}

#[test]
fn yolo_resumes_a_saved_suggest_session_with_host_access() {
    let home = TempDir::new().expect("temp home");
    let cwd = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside workspace");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust workspace");
    let seed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .env_remove("ORCA_SHELL")
        .args(["exec", "--provider", "mock", "--mode", "suggest", "--cwd"])
        .arg(cwd.path())
        .arg("remember this task")
        .output()
        .expect("save suggest session");
    assert!(seed.status.success(), "{seed:?}");
    let marker = outside.path().join("resumed-yolo.txt");
    let prompt = format!("bash echo RESUMED_HOST_ACCESS > \"{}\"", marker.display());
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .env("ORCA_MODE", "suggest")
        .env_remove("ORCA_SHELL")
        .args([
            "exec",
            "resume",
            "--last",
            "--yolo",
            "--output-format",
            "jsonl",
        ])
        .args(["--provider", "mock", "--cwd"])
        .arg(cwd.path())
        .arg(prompt)
        .output()
        .expect("resume with Full Access");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    assert_full_auto_shell(&events);
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
    assert_eq!(read_shell_marker(&marker).trim(), "RESUMED_HOST_ACCESS");
}

#[test]
fn yolo_preserves_an_explicit_shell_deny_rule() {
    let home = TempDir::new().expect("temp home");
    let cwd = TempDir::new().expect("workspace");
    let outside = TempDir::new().expect("outside workspace");
    let marker = outside.path().join("denied-yolo.txt");
    std::fs::write(
        home.path().join("config.toml"),
        "[[permissions.rules]]\ntool = \"bash\"\npattern = \"echo *\"\ndecision = \"deny\"\n",
    )
    .expect("deny rule");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .env_remove("ORCA_SHELL")
        .args(["exec", "--yolo", "--output-format", "jsonl"])
        .args(["--provider", "mock", "--cwd"])
        .arg(cwd.path())
        .arg(format!("bash echo denied > \"{}\"", marker.display()))
        .output()
        .expect("run with deny rule");
    assert_eq!(output.status.code(), Some(3));
    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved" && event["payload"]["decision"] == "deny"
    }));
    assert!(!marker.exists(), "denied shell command ran");
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "approval_required"
    );
}

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
    if host_refuses_restricted_shells() {
        return;
    }
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

    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

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

fn read_shell_marker(path: &std::path::Path) -> String {
    let bytes = std::fs::read(path).expect("shell marker");
    // Windows PowerShell 5.1 redirects text as UTF-16LE; other default shells
    // write these ASCII markers as UTF-8
    if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        assert_eq!(body.len() % 2, 0, "incomplete UTF-16 marker");
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).expect("UTF-16 shell marker")
    } else {
        String::from_utf8(bytes).expect("UTF-8 shell marker")
    }
}

fn assert_full_auto_shell(events: &[Value]) {
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "session.started")
            .expect("session started")["payload"]["approval_mode"],
        "full-auto"
    );
    // JSONL records the policy decision even when it needs no user response.
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved"
            && event["payload"]["decision"] == "allow"
            && event["payload"]["reason"] == "full-auto permits shell"
    }));
    let terminal = events
        .iter()
        .find(|event| event["type"] == "tool.call.completed")
        .expect("shell completed");
    assert_eq!(terminal["payload"]["status"], "completed", "{terminal}");
    assert_eq!(terminal["payload"]["exit_code"], 0, "{terminal}");
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
