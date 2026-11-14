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
fn doctor_reads_a_config_saved_with_crlf_line_ends() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let secret = "sk-test-crlf-config-secret";
    // As a Windows editor saves it: CRLF line ends, also inside a multi-line
    // string.
    let config = format!(
        "# my config\napi_key = {secret:?}\n\n[[hooks]]\nevent = \"post_tool_use\"\ncommand = '''\necho done\n'''\n"
    )
    .replace('\n', "\r\n");
    fs::write(home.join("config.toml"), config).unwrap();

    let output = run_doctor(&home, &cwd, &["--format", "json"]);

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains(secret), "doctor must not print an API key");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let check = |id: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == id)
            .unwrap_or_else(|| panic!("no {id} check in {stdout}"))
            .clone()
    };
    assert_eq!(check("config")["status"], "pass", "{stdout}");
    assert!(
        check("config")["detail"]
            .as_str()
            .unwrap()
            .ends_with("(valid)"),
        "{stdout}"
    );
    assert_eq!(check("credentials")["status"], "pass", "{stdout}");
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

#[test]
fn doctor_reports_which_prices_estimates_use() {
    let as_of = orca_runtime::pricing::rates::builtin_as_of();
    let fetched_at = format!("{as_of}T00:00:00Z");
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let pricing = |output: std::process::Output| -> Value {
        let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "pricing")
            .cloned()
            .expect("pricing check")
    };

    let builtin = pricing(run_doctor(&home, &cwd, &["--format", "json"]));
    assert_eq!(builtin["status"], "pass");
    let detail = builtin["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(&format!(
            "built-in prices as of {as_of}; daily refresh on; DeepSeek "
        )),
        "{detail}"
    );

    let mut cache = serde_json::json!({
        "version": 2,
        "source": "https://models.dev/api.json",
        "fetched_at": fetched_at,
        "models": {"deepseek-flash": {
            "input_cache_miss": 0.2, "input_cache_hit": 0.004, "output": 0.8,
            "updated": as_of.to_string()
        }}
    });
    fs::write(home.join("pricing-cache.json"), cache.to_string()).unwrap();
    fs::write(home.join("config.toml"), "[pricing]\nrefresh = false\n").unwrap();
    let fetched = pricing(run_doctor(&home, &cwd, &["--format", "json"]));
    assert_eq!(fetched["status"], "pass");
    let detail = fetched["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(&format!(
            "https://models.dev/api.json prices fetched {fetched_at}; refresh off; DeepSeek "
        )),
        "{detail}"
    );

    cache["models"]["deepseek-flash"]["updated"] =
        serde_json::json!(as_of.pred_opt().unwrap().to_string());
    fs::write(home.join("pricing-cache.json"), cache.to_string()).unwrap();
    let stale = pricing(run_doctor(&home, &cwd, &["--format", "json"]));
    assert_eq!(stale["status"], "pass");
    let detail = stale["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(&format!(
            "built-in prices as of {as_of}; refresh off; DeepSeek "
        )),
        "{detail}"
    );
}

#[test]
fn doctor_json_shorthand_and_directory_alias_produce_the_stable_report() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir(&cwd).unwrap();
    let legacy = run_doctor(&home, &cwd, &["--format", "json"]);
    let short = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args(["doctor", "--json", "--cd", cwd.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(short.status.code(), legacy.status.code());
    let legacy: Value = serde_json::from_slice(&legacy.stdout).unwrap();
    let short: Value = serde_json::from_slice(&short.stdout).unwrap();
    assert_eq!(short, legacy);
    assert!(!home.exists(), "doctor must remain read-only");
}

#[test]
fn doctor_online_reports_a_rejected_key_without_echoing_it() {
    use std::io::{Read, Write};

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir(&cwd).unwrap();
    let secret = "sk-test-online-secret";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    // Polled, so that a doctor that never asks fails the test instead of
    // leaving the fixture waiting forever.
    let doctor_exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exited = doctor_exited.clone();
    let server = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if exited.load(std::sync::atomic::Ordering::SeqCst)
                        || started.elapsed() > std::time::Duration::from_secs(60)
                    {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept the models request: {error}"),
            }
        };
        // An accepted stream inherits non-blocking mode on macOS.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = [0_u8; 2048];
        let read = stream.read(&mut request).unwrap_or(0);
        let body = r#"{"error":{"message":"Authentication Fails","type":"authentication_error"}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        Some(String::from_utf8_lossy(&request[..read]).into_owned())
    });

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .env("ORCA_API_KEY", secret)
        .env("ORCA_BASE_URL", &base_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "doctor",
            "--online",
            "--json",
            "--cwd",
            cwd.to_str().unwrap(),
        ])
        .output()
        .expect("run orca doctor --online");
    doctor_exited.store(true, std::sync::atomic::Ordering::SeqCst);

    let request = server
        .join()
        .unwrap()
        .expect("doctor --online asks the endpoint for its models");
    assert!(
        request.starts_with("GET /v1/models HTTP/1.1\r\n"),
        "{request}"
    );
    assert_eq!(output.status.code(), Some(1), "a rejected key fails");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains(secret),
        "doctor must never print an API key"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let online = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "credentials-online")
        .expect("credentials-online check");
    assert_eq!(online["status"], "fail", "{online}");
    assert!(
        online["detail"].as_str().unwrap().contains("HTTP 401"),
        "{online}"
    );
    assert!(!home.exists(), "doctor --online must remain read-only");
}
