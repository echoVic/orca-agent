use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

fn run_doctor(
    home: &std::path::Path,
    cwd: &std::path::Path,
    extra: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .args(["doctor", "--cwd", cwd.to_str().expect("cwd is UTF-8")])
        .args(extra)
        .output()
        .expect("run orca doctor")
}

#[test]
fn doctor_is_a_real_read_only_command_with_stable_json() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let before = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();

    let output = run_doctor(&home, &cwd, &["--format", "json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stderr.is_empty(),
        "doctor must keep diagnostics on stdout"
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("stable doctor JSON");
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["package"], "@blade-ai/orca");
    assert_eq!(report["website"], "https://orcaagent.dev");
    assert_eq!(report["cwd"]["requested"], cwd.to_str().unwrap());
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["id"] == "credentials" && check["status"] == "fail" })
    );
    assert!(
        serde_json::to_string(&report)
            .unwrap()
            .contains("credentials")
    );

    let after = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        before, after,
        "doctor must not create ORCA_HOME or other files"
    );
}

#[test]
fn doctor_redacts_environment_credentials_and_reports_their_source() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let secret = "sk-test-doctor-secret";

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .env("ORCA_API_KEY", secret)
        .args(["doctor", "--cwd", cwd.to_str().unwrap(), "--format", "json"])
        .output()
        .expect("run orca doctor");

    // Hosts without an OS sandbox may report a required sandbox failure, but
    // credential detection and redaction must remain deterministic.
    assert!(matches!(output.status.code(), Some(0) | Some(1)));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains(secret),
        "doctor must never print an API key"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let credentials = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "credentials")
        .unwrap();
    assert_eq!(credentials["status"], "pass");
    assert!(
        credentials["detail"]
            .as_str()
            .unwrap()
            .contains("ORCA_API_KEY")
    );
}

#[test]
fn doctor_reports_config_file_credentials_without_echoing_them() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let secret = "sk-test-config-secret";
    fs::write(home.join("config.toml"), format!("api_key = {secret:?}\n")).unwrap();

    let output = run_doctor(&home, &cwd, &["--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains(secret), "doctor must not print an API key");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let credentials = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "credentials")
        .unwrap();
    assert_eq!(credentials["status"], "pass");
    assert!(
        credentials["detail"]
            .as_str()
            .unwrap()
            .contains("config.toml")
    );
}

#[test]
fn doctor_reports_malformed_config_without_starting_the_tui() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let secret = "sk-test-malformed-config-secret";
    fs::write(
        home.join("config.toml"),
        format!("api_key = {secret:?}\nmode = [broken\n"),
    )
    .unwrap();

    let output = run_doctor(&home, &cwd, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("config"));
    assert!(!stdout.contains(secret));
    assert!(!stdout.contains("Welcome to Orca"));
}
