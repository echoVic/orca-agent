use std::ffi::{OsStr, OsString};
use std::io::Write;
#[cfg(not(windows))]
use std::io::{BufRead, BufReader};
#[cfg(not(windows))]
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use orca_runtime::history::SessionStore;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

#[path = "support/sandbox_test_parent.rs"]
mod sandbox_test_support;
#[path = "support/server_test_client.rs"]
mod server_test_client;

use sandbox_test_support::sandbox_test_parent;
use server_test_client::ServerTestClient;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct OrcaCommand {
    command: Command,
    home: Option<TempDir>,
    trust_config_dir: PathBuf,
    trusted_folders: Vec<PathBuf>,
}

impl OrcaCommand {
    fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect::<Vec<OsString>>();
        for pair in args.windows(2) {
            if pair[0] == OsStr::new("--cwd") {
                let folder = PathBuf::from(&pair[1]);
                trust_test_folder(&folder, &self.trust_config_dir);
                if !self.trusted_folders.contains(&folder) {
                    self.trusted_folders.push(folder);
                }
            }
        }
        self.command.args(&args);
        self
    }

    fn env<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        if key.as_ref() == OsStr::new("ORCA_HOME") {
            self.home.take();
            self.trust_config_dir = PathBuf::from(value.as_ref());
            trust_all_test_folders(&self.trust_config_dir);
            for folder in &self.trusted_folders {
                trust_test_folder(folder, &self.trust_config_dir);
            }
        }
        self.command.env(key, value);
        self
    }

    fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stdin(cfg);
        self
    }

    fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stdout(cfg);
        self
    }

    fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stderr(cfg);
        self
    }

    fn sandbox_workspace(&mut self, folder: &Path) -> &mut Self {
        trust_test_folder(folder, &self.trust_config_dir);
        if !self.trusted_folders.iter().any(|trusted| trusted == folder) {
            self.trusted_folders.push(folder.to_path_buf());
        }
        self
    }

    fn spawn(&mut self) -> std::io::Result<ServerTestClient> {
        #[cfg(windows)]
        {
            let capabilities = orca_windows_sandbox::CapabilityStore::new(
                self.trust_config_dir.join("windows-capabilities"),
            );
            for folder in &self.trusted_folders {
                capabilities
                    .provision_setup(folder, orca_windows_sandbox::SETUP_HELPER_VERSION)
                    .expect("provision Windows sandbox setup for server contract workspace");
            }
        }
        ServerTestClient::spawn(&mut self.command, self.home.take())
    }

    fn get_envs(&self) -> std::process::CommandEnvs<'_> {
        self.command.get_envs()
    }
}

fn orca_command() -> OrcaCommand {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    let home = tempfile::Builder::new()
        .prefix("orca-server-contract-")
        .tempdir()
        .expect("create isolated ORCA_HOME");
    trust_all_test_folders(home.path());
    command.env("ORCA_HOME", home.path());
    let trust_config_dir = home.path().to_path_buf();
    let current_dir = std::env::current_dir().expect("current test directory");
    trust_test_folder(&current_dir, &trust_config_dir);
    OrcaCommand {
        command,
        home: Some(home),
        trust_config_dir,
        trusted_folders: vec![current_dir],
    }
}

fn trust_all_test_folders(home: &Path) {
    trust_test_folder(Path::new("/"), home);
}

fn trust_test_folder(folder: &Path, home: &Path) {
    orca_core::config::folder_trust::set_trust_with_config_dir(
        folder,
        home,
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust server contract workspaces");
}

fn platform_shell_script<'a>(unix: &'a str, windows: &'a str) -> &'a str {
    #[cfg(windows)]
    {
        let _ = unix;
        windows
    }
    #[cfg(not(windows))]
    {
        let _ = windows;
        unix
    }
}

fn platform_command(unix: &str, windows: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let _ = unix;
        vec![
            "pwsh.exe".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            windows.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        let _ = windows;
        vec!["sh".to_string(), "-lc".to_string(), unix.to_string()]
    }
}

fn command_exec_request(id: &str, unix: &str, windows: &str, mut params: Value) -> Value {
    let params = params
        .as_object_mut()
        .expect("command/exec fixture params must be an object");
    assert!(
        params
            .insert(
                "command".to_string(),
                json!(platform_command(unix, windows))
            )
            .is_none(),
        "command/exec fixture command must be owned by the platform helper"
    );
    json!({"id": id, "method": "command/exec", "params": params})
}

fn command_exec_request_for_platform_script(id: &str, script: &str, params: Value) -> Value {
    #[cfg(windows)]
    {
        command_exec_request(id, "", script, params)
    }
    #[cfg(not(windows))]
    {
        command_exec_request(id, script, "", params)
    }
}

fn unix_command_exec_request(id: &str, unix: &str, params: Value) -> Value {
    command_exec_request(
        id,
        unix,
        "throw 'Unix command fixture reached Windows execution'",
        params,
    )
}

fn platform_fixture_command(unix: &str, windows_node: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let _ = unix;
        vec![
            "node".to_string(),
            "-e".to_string(),
            windows_node.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        let _ = windows_node;
        platform_command(unix, "")
    }
}

fn platform_fixture_command_with_args(
    unix: &str,
    windows_node: &str,
    unix_args: &[&str],
) -> Vec<String> {
    #[cfg(windows)]
    {
        let _ = unix_args;
        platform_fixture_command(unix, windows_node)
    }
    #[cfg(not(windows))]
    {
        let _ = windows_node;
        platform_command_with_args(unix, "", unix_args)
    }
}

fn javascript_path(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("serialize JavaScript fixture path")
}

fn platform_command_with_args(unix: &str, windows: &str, unix_args: &[&str]) -> Vec<String> {
    #[cfg(windows)]
    {
        let _ = unix_args;
        platform_command(unix, windows)
    }
    #[cfg(not(windows))]
    {
        let mut command = platform_command(unix, windows);
        command.push("sh".to_string());
        command.extend(unix_args.iter().map(|arg| (*arg).to_string()));
        command
    }
}

fn powershell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn unix_shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn platform_write_file_command(path: &Path, content: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            "Set-Content -NoNewline -LiteralPath {} -Value '{}'",
            powershell_path(path),
            content.replace('\'', "''")
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "printf '%s' '{}' > {}",
            content.replace('\'', "'\\''"),
            unix_shell_path(path)
        )
    }
}

#[test]
fn server_test_commands_isolate_orca_home() {
    let command = orca_command();
    let orca_home = command
        .get_envs()
        .find_map(|(key, value)| (key == "ORCA_HOME").then_some(value).flatten())
        .map(PathBuf::from)
        .expect("isolated ORCA_HOME");

    assert!(orca_home.exists(), "isolated ORCA_HOME must exist");
    drop(command);
    assert!(
        !orca_home.exists(),
        "isolated ORCA_HOME must be removed when its command guard is dropped"
    );
}

#[test]
fn server_mode_accepts_submit_and_streams_protocol_events() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":1,"op":"submit","prompt":"hello from server"}}"#
        )
        .expect("write submit request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 4);
    assert!(events.iter().all(|event| event["id"] == 1));
    assert!(events.iter().all(|event| event.get("type").is_none()));

    assert!(has_event(&events, "turn_started"));
    assert!(has_event(&events, "reasoning_delta"));
    assert!(has_event(&events, "message_delta"));

    let completed = events
        .iter()
        .find(|event| event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success");
}

#[test]
fn server_mode_clean_eof_waits_for_slow_stateless_submit_terminal() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 2.5);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    writeln!(
        child.stdin_mut(),
        r#"{{"id":"slow-submit","op":"submit","prompt":"slow stateless submit"}}"#
    )
    .expect("write submit request");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "slow-submit" && event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success", "events={events:?}");
    assert!(has_event(&events, "message_delta"));
    assert!(
        events
            .iter()
            .all(|event| event["event"] != "thread_started"),
        "stateless submit exposed a recorded thread"
    );
}

#[test]
fn server_mode_clean_eof_terminalizes_stateless_submit_waiting_for_client_input() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    writeln!(
        child.stdin_mut(),
        r#"{{"id":"input-submit","op":"submit","prompt":"ask Continue?"}}"#
    )
    .expect("write submit request");
    child.close_stdin();

    let output = wait_for_child_output_with_timeout(child, Duration::from_secs(5))
        .expect("server exited after unreachable stateless input waiter failed");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .filter(|event| event["id"] == "input-submit" && event["event"] == "turn_completed")
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1, "events={events:?}");
    assert_eq!(completed[0]["status"], "cancelled", "events={events:?}");
}

#[test]
fn server_mode_accepts_turn_start_method_and_streams_protocol_events() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello from turn start"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 4);
    assert!(events.iter().all(|event| event["id"] == "req-1"));
    assert!(events.iter().all(|event| event.get("type").is_none()));
    assert!(has_event(&events, "turn_started"));
    assert!(has_event(&events, "reasoning_delta"));
    assert!(has_event(&events, "message_delta"));

    let completed = events
        .iter()
        .find(|event| event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success");
}

#[test]
fn server_mode_streams_multi_root_fuzzy_file_search_sessions() {
    let first = tempdir().expect("first root");
    let second = tempdir().expect("second root");
    std::fs::create_dir_all(first.path().join("src")).expect("first src");
    std::fs::create_dir_all(second.path().join("src")).expect("second src");
    std::fs::write(first.path().join("src/main.rs"), "first").expect("first file");
    std::fs::write(second.path().join("src/main.rs"), "second").expect("second file");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "search-start",
                "method": "fuzzyFileSearch/sessionStart",
                "params": {
                    "sessionId": "files-1",
                    "roots": [first.path(), second.path()],
                    "resultLimit": 32,
                }
            })
        )
        .expect("write search start");
        stdin.flush().expect("flush search start");
    }
    let started = child.expect_event("search-start", "fuzzy_file_search_session_started");
    assert_eq!(started["event"], "fuzzy_file_search_session_started");
    assert_eq!(started["sessionId"], "files-1");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "search-update",
                "method": "fuzzyFileSearch/sessionUpdate",
                "params": {"sessionId": "files-1", "query": "main"}
            })
        )
        .expect("write search update");
        stdin.flush().expect("flush search update");
    }

    let accepted = child.expect_event("search-update", "fuzzy_file_search_session_update_accepted");
    assert_eq!(
        accepted["event"],
        "fuzzy_file_search_session_update_accepted"
    );
    let final_update = child.expect_event_matching(
        "search-start",
        "fuzzy_file_search_session_updated",
        |event| event["query"] == "main" && event["phase"] == "complete",
    );
    assert_eq!(final_update["method"], "fuzzyFileSearch/sessionUpdated");
    let files = final_update["files"].as_array().expect("files");
    assert_eq!(
        files
            .iter()
            .filter(|file| file["path"] == "src/main.rs")
            .count(),
        2
    );
    let first_root = first.path().canonicalize().expect("canonical first root");
    let second_root = second.path().canonicalize().expect("canonical second root");
    assert!(
        files
            .iter()
            .any(|file| { file["root"].as_str() == Some(first_root.to_string_lossy().as_ref()) })
    );
    assert!(
        files
            .iter()
            .any(|file| { file["root"].as_str() == Some(second_root.to_string_lossy().as_ref()) })
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "search-stop",
                "method": "fuzzyFileSearch/sessionStop",
                "params": {"sessionId": "files-1"}
            })
        )
        .expect("write search stop");
        stdin.flush().expect("flush search stop");
    }
    let stopped = child.expect_event("search-stop", "fuzzy_file_search_session_stopped");
    assert_eq!(stopped["event"], "fuzzy_file_search_session_stopped");
    assert_eq!(stopped["sessionId"], "files-1");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_eof_joins_active_fuzzy_file_search_sessions() {
    let workspace = tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join("src")).expect("src directory");
    for index in 0..256 {
        std::fs::write(
            workspace
                .path()
                .join("src")
                .join(format!("file-{index}.rs")),
            "fn main() {}\n",
        )
        .expect("search fixture");
    }

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "search-start",
                "method": "fuzzyFileSearch/sessionStart",
                "params": {
                    "sessionId": "files-eof",
                    "roots": [workspace.path()],
                    "resultLimit": 32,
                }
            })
        )
        .expect("write search start");
        stdin.flush().expect("flush search start");
    }
    child.expect_event("search-start", "fuzzy_file_search_session_started");

    child.close_stdin();
    let started = Instant::now();
    let output = child
        .wait_with_output_timeout(Duration::from_secs(3))
        .expect("server must join active search on EOF");

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "active search shutdown exceeded deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_streams_unified_file_skill_and_plugin_mention_candidates() {
    let workspace = tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname='mention-server-test'\nversion='0.1.0'\n",
    )
    .expect("workspace manifest");
    std::fs::write(workspace.path().join("review.md"), "review file").expect("review file");
    let skill_dir = workspace.path().join(".orca/skills/review");
    std::fs::create_dir_all(&skill_dir).expect("skill directory");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Review\ndescription: Review changes safely\n---\n\nReview the diff.\n",
    )
    .expect("skill manifest");
    let plugin_dir = workspace.path().join(".orca/plugins/review/.codex-plugin");
    std::fs::create_dir_all(&plugin_dir).expect("plugin directory");
    std::fs::write(
        plugin_dir.join("plugin.json"),
        r#"{"name":"review-plugin","description":"Review plugin","interface":{"displayName":"review"}}"#,
    )
    .expect("plugin manifest");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "thread-start",
                "method": "thread/start",
                "params": {"runtimeWorkspaceRoots": [workspace.path()]}
            })
        )
        .expect("write thread start");
        stdin.flush().expect("flush thread start");
    }
    let thread_started = child.expect_event("thread-start", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-start",
                "method": "mention/search/start",
                "params": {
                    "sessionId": "mentions-1",
                    "threadId": thread_id,
                    "resultLimit": 32
                }
            })
        )
        .expect("write mention search start");
        stdin.flush().expect("flush mention search start");
    }
    let started = child.expect_event("mention-start", "mention_search_session_started");
    assert_eq!(started["sessionId"], "mentions-1");
    assert_eq!(started["threadId"], thread_id);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-update",
                "method": "mention/search/update",
                "params": {"sessionId": "mentions-1", "query": "review"}
            })
        )
        .expect("write mention search update");
        stdin.flush().expect("flush mention search update");
    }
    let accepted = child.expect_event("mention-update", "mention_search_session_update_accepted");
    assert_eq!(accepted["query"], "review");

    let completed =
        child.expect_event_matching("mention-start", "mention_search_session_updated", |event| {
            event["query"] == "review" && event["phase"] == "complete"
        });
    assert_eq!(completed["method"], "mention/search/updated");
    let candidates = completed["candidates"].as_array().expect("candidates");
    for kind in ["file", "skill", "plugin"] {
        assert!(
            candidates.iter().any(|candidate| candidate["kind"] == kind),
            "missing {kind} candidate: {candidates:?}"
        );
    }
    let review_ids = candidates
        .iter()
        .filter(|candidate| candidate["display"] == "review" || candidate["display"] == "review.md")
        .filter_map(|candidate| candidate["id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        review_ids.len(),
        3,
        "candidate identities must remain atomic"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-stop",
                "method": "mention/search/stop",
                "params": {"sessionId": "mentions-1"}
            })
        )
        .expect("write mention search stop");
        stdin.flush().expect("flush mention search stop");
    }
    let stopped = child.expect_event("mention-stop", "mention_search_session_stopped");
    assert_eq!(stopped["sessionId"], "mentions-1");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_unified_mention_discovers_and_expands_mcp_resource() {
    let _guard = lock_env();
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_resource_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[[mcp_servers]]\nname = \"resources\"\ntransport = \"stdio\"\ncommand = \"{}\"\n",
            server.display()
        ),
    )
    .expect("write config");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "thread-start",
                "method": "thread/start",
                "params": {"runtimeWorkspaceRoots": [workspace.path()]}
            })
        )
        .expect("write thread start");
        stdin.flush().expect("flush thread start");
    }
    let thread_started = child.expect_event("thread-start", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-start",
                "method": "mention/search/start",
                "params": {
                    "sessionId": "mentions-mcp",
                    "threadId": thread_id,
                    "resultLimit": 12
                }
            })
        )
        .expect("write mention start");
        stdin.flush().expect("flush mention start");
    }
    child.expect_event("mention-start", "mention_search_session_started");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-update",
                "method": "mention/search/update",
                "params": {"sessionId": "mentions-mcp", "query": "memo"}
            })
        )
        .expect("write mention update");
        stdin.flush().expect("flush mention update");
    }
    child.expect_event("mention-update", "mention_search_session_update_accepted");

    let resource_update =
        child.expect_event_matching("mention-start", "mention_search_session_updated", |event| {
            event["query"] == "memo"
                && event["candidates"].as_array().is_some_and(|candidates| {
                    candidates
                        .iter()
                        .any(|candidate| candidate["kind"] == "resource")
                })
        });
    let resource_target = resource_update["candidates"]
        .as_array()
        .and_then(|candidates| {
            candidates
                .iter()
                .find(|candidate| candidate["kind"] == "resource")
        })
        .map(|candidate| candidate["target"].clone())
        .expect("resource mention target");
    assert_eq!(resource_target["type"], "resource");
    assert_eq!(resource_target["server"], "resources");
    assert_eq!(resource_target["uri"], "memo://orca/one");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "mention-stop",
                "method": "mention/search/stop",
                "params": {"sessionId": "mentions-mcp"}
            })
        )
        .expect("write mention stop");
        stdin.flush().expect("flush mention stop");
    }
    child.expect_event("mention-stop", "mention_search_session_stopped");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-1",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [
                        {"type": "text", "text": "inspect "},
                        {"type": "mention", "name": "memo one", "target": resource_target}
                    ]
                }
            })
        )
        .expect("write resource mention turn");
        stdin.flush().expect("flush resource mention turn");
    }
    child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-2",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            })
        )
        .expect("write history echo turn");
        stdin.flush().expect("flush history echo turn");
    }
    let events = child.drain_events_until_event("turn-2", "turn_completed");
    let echoed = events
        .iter()
        .filter(|event| event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        echoed.contains("resource body from shared registry"),
        "resource content should enter model history: {echoed}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_streams_agent_message_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello item stream"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let item_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "agent_message")
        .expect("agent message item_started");
    let item_id = item_started["item"]["id"].as_str().expect("item id");
    assert_eq!(item_started["item"]["text"], "");

    let item_delta = events
        .iter()
        .find(|event| event["event"] == "item_message_delta" && event["itemId"] == item_id)
        .expect("agent message item delta");
    assert!(
        item_delta["delta"]
            .as_str()
            .is_some_and(|delta| delta.contains("Mock runtime completed"))
    );

    let item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("agent message item_completed");
    assert!(
        item_completed["item"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Mock runtime completed"))
    );
}

#[test]
fn server_mode_streams_reasoning_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello reasoning item stream"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let item_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "reasoning")
        .expect("reasoning item_started");
    let item_id = item_started["item"]["id"].as_str().expect("item id");
    assert_eq!(item_started["item"]["summary"], "");
    assert_eq!(item_started["item"]["content"], "");

    let item_delta = events
        .iter()
        .find(|event| event["event"] == "item_reasoning_delta" && event["itemId"] == item_id)
        .expect("reasoning item delta");
    assert!(
        item_delta["delta"]
            .as_str()
            .is_some_and(|delta| delta.contains("DeepSeek reasoning channel"))
    );

    let item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("reasoning item_completed");
    assert_eq!(item_completed["item"]["type"], "reasoning");
    assert!(
        item_completed["item"]["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("DeepSeek reasoning channel"))
    );
    assert_eq!(item_completed["item"]["content"], "");
    assert!(has_event(&events, "reasoning_delta"));
}

#[test]
fn server_mode_streams_tool_call_item_lifecycle() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"bash printf hi"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let started = events
        .iter()
        .find(|event| {
            event["event"] == "item_started" && event["item"]["type"] == "commandExecution"
        })
        .expect("tool item_started");
    let item_id = started["item"]["id"].as_str().expect("item id");
    assert_eq!(started["item"]["tool"], "bash");
    assert_eq!(started["item"]["command"], "printf hi");
    assert_eq!(started["item"]["status"], "in_progress");

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("tool item_completed");
    assert_eq!(completed["item"]["type"], "commandExecution");
    assert_eq!(completed["item"]["status"], "completed");
    assert!(
        completed["item"]["aggregatedOutput"]
            .as_str()
            .is_some_and(|output| output.contains("hi"))
    );
    assert!(completed["item"].get("output").is_none());

    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
fn server_mode_streams_file_change_item_lifecycle_for_edit() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n[[permissions.rules]]\ntool = \"edit\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let file_path = workspace.path().join("note.txt");
    std::fs::write(&file_path, "hello orca\n").expect("write fixture");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"edit-req","method":"turn/start","params":{{"input":[{{"type":"text","text":"edit note.txt :: hello => hi"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hi orca\n");

    let events = parse_jsonl(&output.stdout);
    let started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "fileChange")
        .expect("file_change item_started");
    let item_id = started["item"]["id"].as_str().expect("file_change item id");
    assert!(started["item"].get("tool").is_none());
    assert_eq!(started["item"]["status"], "inProgress");
    assert_eq!(started["item"]["changes"][0]["path"], "note.txt");
    assert_eq!(started["item"]["changes"][0]["kind"], "edit");
    assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("file_change item_completed");
    assert_eq!(completed["item"]["type"], "fileChange");
    assert_eq!(completed["item"]["status"], "completed");
    assert!(completed["item"].get("output").is_none());
    assert!(completed["item"].get("error").is_none());
    assert!(completed["item"].get("tool").is_none());
    assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
    assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
    assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
fn server_mode_streams_plan_updated_notification() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"plan implementing todo support"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let plan = events
        .iter()
        .find(|event| event["event"] == "turn_plan_updated")
        .expect("turn_plan_updated event");
    assert!(plan["threadId"].is_null());
    assert!(plan["turnId"].is_null());
    assert_eq!(plan["explanation"], "implementing todo support");
    assert_eq!(plan["plan"][0]["step"], "Inspect references");
    assert_eq!(plan["plan"][0]["status"], "completed");
    assert_eq!(plan["plan"][1]["step"], "Implement task plan support");
    assert_eq!(plan["plan"][1]["status"], "in_progress");
    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
#[cfg(unix)]
fn server_mode_streams_external_tool_as_dynamic_tool_call_item() {
    use std::os::unix::fs::PermissionsExt;

    with_orca_home(|home| {
        let tools_dir = home.join("tools");
        std::fs::create_dir_all(&tools_dir).expect("tools dir");
        let workspace = home.join("workspace");
        let scripts_dir = workspace.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
        let output_file = workspace.join("deploy-output.txt");
        let script = scripts_dir.join("deploy.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > {}\nprintf 'deployed staging'\n",
                shell_escape(&output_file)
            ),
        )
        .expect("write deploy script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod deploy script");
        std::fs::write(
            tools_dir.join("deploy.toml"),
            r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { env = { type = "string", description = "environment" } }
"#,
        )
        .expect("write deploy descriptor");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().expect("workspace path"),
            ])
            .env("ORCA_MODE", "full-auto")
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"external deploy {{\"env\":\"staging\"}}"}}]}}}}"#
            )
            .expect("write turn/start request");
        }

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("external tool stdin"),
            r#"{"env":"staging"}"#
        );

        let events = parse_jsonl(&output.stdout);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "mock-tool-1"
            })
            .expect("external dynamic item_started");
        assert!(started["item"]["namespace"].is_null());
        assert_eq!(started["item"]["tool"], "deploy");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["env"], "staging");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "mock-tool-1"
            })
            .expect("external dynamic item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["success"], true);
        assert_eq!(
            completed["item"]["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(completed["item"]["error"].is_null());
    });
}

#[test]
#[cfg(unix)]
fn server_mode_projects_failed_external_tool_metadata_in_thread_items() {
    use std::os::unix::fs::PermissionsExt;

    with_orca_home(|home| {
        let tools_dir = home.join("tools");
        std::fs::create_dir_all(&tools_dir).expect("tools dir");
        let workspace = home.join("workspace");
        let scripts_dir = workspace.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
        let script = scripts_dir.join("deploy.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'deploy failed' >&2\nexit 42\n")
            .expect("write failing deploy script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod deploy script");
        std::fs::write(
            tools_dir.join("deploy.toml"),
            r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { env = { type = "string", description = "environment" } }
"#,
        )
        .expect("write deploy descriptor");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().expect("workspace path"),
            ])
            .env("ORCA_MODE", "full-auto")
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = child.expect_event("thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"external deploy {{\"env\":\"staging\"}}"}}]}}}}"#,
                thread_id
            )
            .expect("write failing external turn");
            stdin.flush().expect("flush failing external turn");
        }
        child.expect_event("turn-1", "turn_completed");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
                thread_id
            )
            .expect("write thread/items/list");
        }
        child.close_stdin();

        let items = child.expect_event("items", "thread_items_list");
        let item_data = items["data"].as_array().expect("thread items data");
        let external_item = item_data
            .iter()
            .find(|item| item["item"]["id"] == "mock-tool-1")
            .expect("external item");
        assert_eq!(external_item["item"]["type"], "dynamicToolCall");
        assert_eq!(external_item["item"]["status"], "failed");
        assert_eq!(external_item["item"]["success"], false);
        assert!(
            external_item["item"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("deploy failed"))
        );
        assert_eq!(external_item["item"]["error"]["exitCode"], 42);
        assert!(external_item["item"]["contentItems"].is_null());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_streams_workflow_item_lifecycle() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    child.set_event_timeout(Duration::from_secs(30));

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"workflow-turn","method":"turn/start","params":{{"input":[{{"type":"text","text":"workflow inline"}}]}}}}"#
        )
        .expect("write workflow turn");
        stdin.flush().expect("flush workflow turn");
    }

    let events = read_events_until_workflow_item_completed(&mut child, "workflow-turn");
    let started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "workflow")
        .expect("workflow item_started");
    let workflow_id = started["item"]["id"].as_str().expect("workflow item id");
    assert_eq!(started["item"]["status"], "running");
    assert_eq!(started["item"]["workflowName"], "mock-workflow");

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == workflow_id)
        .expect("workflow item_completed");
    assert_eq!(completed["item"]["type"], "workflow");
    assert_eq!(
        completed["item"]["status"], "completed",
        "workflow events: {events:?}"
    );
    assert_eq!(completed["item"]["workflowName"], "mock-workflow");
    assert!(
        completed["item"]["result"]
            .as_str()
            .is_some_and(|result| result.contains("Workflow completed"))
    );
    assert!(events.iter().any(|event| {
        event["event"] == "workflow_result_available"
            && event["result"]
                .as_str()
                .is_some_and(|result| result.contains("Workflow completed"))
    }));

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn server_mode_streams_proposed_plan_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"mock_proposed_plan"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let plan_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "plan")
        .expect("plan item_started");
    let plan_id = plan_started["item"]["id"].as_str().expect("plan id");
    assert_eq!(plan_started["item"]["text"], "");

    let plan_delta = events
        .iter()
        .find(|event| event["event"] == "item_plan_delta" && event["itemId"] == plan_id)
        .expect("plan delta");
    assert_eq!(plan_delta["delta"], "# Final plan\n- first\n- second\n");

    let plan_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == plan_id)
        .expect("plan item_completed");
    assert_eq!(plan_completed["item"]["type"], "plan");
    assert_eq!(
        plan_completed["item"]["text"],
        "# Final plan\n- first\n- second\n"
    );

    let agent_completed = events
        .iter()
        .find(|event| {
            event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
        })
        .expect("agent message item_completed");
    assert_eq!(agent_completed["item"]["text"], "Preface\n\nPostscript");
    assert!(has_event(&events, "message_delta"));
}

#[test]
fn server_mode_replays_completed_message_and_reasoning_items_after_restart() {
    assert_completed_model_items_replay_after_restart(
        "mock_usage",
        &["agent_message", "reasoning"],
    );
}

#[test]
fn server_mode_replays_completed_proposed_plan_items_after_restart() {
    assert_completed_model_items_replay_after_restart(
        "mock_proposed_plan",
        &["agent_message", "plan"],
    );
}

fn assert_completed_model_items_replay_after_restart(prompt: &str, expected_types: &[&str]) {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().expect("workspace path"),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn live orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"{}"}}]}}}}"#,
            thread_id, prompt
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let mut saw_turn_completed = false;
    let mut completed_types = Vec::new();
    let turn_events =
        child.drain_events_until_matching("completed model item lifecycle", |event| {
            if event["id"] != "turn" {
                return false;
            }
            if event["event"] == "turn_completed" {
                saw_turn_completed = true;
            }
            if event["event"] == "item_completed"
                && let Some(item_type) = event["item"]["type"].as_str()
                && expected_types.contains(&item_type)
            {
                completed_types.push(item_type.to_string());
            }
            saw_turn_completed
                && expected_types
                    .iter()
                    .all(|expected| completed_types.iter().any(|actual| actual == expected))
        });
    let completed_items = expected_types
        .iter()
        .map(|expected_type| {
            let completed = turn_events
                .iter()
                .find(|event| {
                    event["event"] == "item_completed" && event["item"]["type"] == *expected_type
                })
                .unwrap_or_else(|| {
                    panic!("missing live {expected_type} completion: {turn_events:?}")
                });
            let item = completed["item"].clone();
            let item_id = item["id"].as_str().expect("completed item id");
            assert!(
                !matches!(
                    item_id,
                    "item-agent-message-1" | "item-reasoning-1" | "item-plan-1"
                ),
                "completed model item retained a static live id: {item}"
            );
            assert!(turn_events.iter().any(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == *expected_type
                    && event["item"]["id"] == item_id
            }));
            item
        })
        .collect::<Vec<_>>();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"live-items","method":"thread/items/list","params":{{"threadId":"{}","limit":20}}}}"#,
            thread_id
        )
        .expect("write live thread/items/list");
        stdin.flush().expect("flush live thread/items/list");
    }
    let live = child.expect_event("live-items", "thread_items_list");
    assert_completed_items_match_projection(&completed_items, &live);

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for live server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "live server stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut cold = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().expect("workspace path"),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cold orca server");
    {
        let stdin = cold.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cold-items","method":"thread/items/list","params":{{"threadId":"{}","limit":20}}}}"#,
            thread_id
        )
        .expect("write cold thread/items/list");
    }
    cold.close_stdin();
    let cold_items = cold.expect_event("cold-items", "thread_items_list");
    assert_completed_items_match_projection(&completed_items, &cold_items);

    let output = cold.wait_with_output().expect("wait for cold server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "cold server stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_completed_items_match_projection(completed_items: &[Value], projection: &Value) {
    let projected = projection["data"]
        .as_array()
        .expect("thread item projection data");
    for completed in completed_items {
        let item_id = completed["id"].as_str().expect("completed item id");
        let item_type = completed["type"].as_str().expect("completed item type");
        let stored = projected
            .iter()
            .find(|entry| entry["item"]["type"] == item_type)
            .unwrap_or_else(|| panic!("missing projected {item_type} item: {projection}"));
        assert_eq!(stored["itemId"], item_id);
        assert_eq!(&stored["item"], completed);
    }
}

#[test]
fn server_mode_accepts_thread_start_method_and_returns_thread_event() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], "thread-req");
    assert_eq!(events[0]["event"], "thread_started");
    assert!(
        events[0]["threadId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
}

#[test]
fn server_mode_accepts_idle_turn_control_methods() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt","method":"turn/interrupt","params":{{"turnId":"turn-missing"}}}}"#
        )
        .expect("write turn/interrupt");
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"turn/resume","params":{{"turnId":"turn-missing"}}}}"#
        )
        .expect("write turn/resume");
        writeln!(
            stdin,
            r#"{{"id":"steer","method":"turn/steer","params":{{"turnId":"turn-missing","input":[{{"type":"text","text":"please continue differently"}}]}}}}"#
        )
        .expect("write turn/steer");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["id"], "interrupt");
    assert_eq!(events[0]["event"], "turn_controlled");
    assert_eq!(events[0]["action"], "interrupt");
    assert_eq!(events[0]["turnId"], "turn-missing");
    assert_eq!(events[0]["status"], "idle");

    assert_eq!(events[1]["id"], "resume");
    assert_eq!(events[1]["event"], "turn_controlled");
    assert_eq!(events[1]["action"], "resume");
    assert_eq!(events[1]["turnId"], "turn-missing");
    assert_eq!(events[1]["status"], "idle");

    assert_eq!(events[2]["id"], "steer");
    assert_eq!(events[2]["event"], "turn_controlled");
    assert_eq!(events[2]["action"], "steer");
    assert_eq!(events[2]["turnId"], "turn-missing");
    assert_eq!(events[2]["status"], "idle");
    assert_eq!(events[2]["input"], "please continue differently");
}

#[test]
fn server_mode_interrupts_active_thread_turn_before_completion() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow active turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = child.expect_event("turn-slow", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();

    let interrupt_sent_at = Instant::now();
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-active","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt = child.expect_event("interrupt-active", "turn_controlled");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(500),
        "interrupt was not handled while turn was active"
    );
    assert_eq!(interrupt["action"], "interrupt");
    assert_eq!(interrupt["turnId"], turn_id);
    assert_eq!(interrupt["status"], "interrupted");

    child.close_stdin();
    let completed = child.expect_event("turn-slow", "turn_completed");
    assert_eq!(completed["status"], "cancelled");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_interrupt_cancels_active_pre_model_hook_wait() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 5.0);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-hook","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"cancel hook wait"}}]}}}}"#,
            thread_id
        )
        .expect("write hook turn");
        stdin.flush().expect("flush hook turn");
    }
    let turn_started = child.expect_event("turn-hook", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-hook","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = child.expect_event("interrupt-hook", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = child.expect_event("turn-hook", "turn_completed");
    let interrupt_elapsed = interrupt_sent_at.elapsed();
    assert!(
        interrupt_elapsed < Duration::from_secs(3),
        "turn completion waited too long for pre_model hook cancellation: {interrupt_elapsed:?}"
    );
    assert_eq!(completed["status"], "cancelled");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn server_mode_interrupt_cancels_active_bash_tool_wait_and_accepts_next_turn() {
    let workspace = tempdir().expect("workspace");
    let invocation_marker = workspace.path().join("bash-invocation-started");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let command = platform_shell_script(
            "touch bash-invocation-started; sleep 5; printf after",
            "New-Item -ItemType File -Force -Path 'bash-invocation-started' | Out-Null; Start-Sleep -Seconds 5; Write-Host -NoNewline 'after'",
        );
        let request = json!({
            "id": "turn-bash",
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": format!("bash {command}")}]
            }
        });
        let stdin = child.stdin_mut();
        writeln!(stdin, "{request}").expect("write bash turn");
        stdin.flush().expect("flush bash turn");
    }
    let turn_started = child.expect_event("turn-bash", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();
    let tool_requested = child.expect_event("turn-bash", "tool_requested");
    assert_eq!(tool_requested["tool"], "bash");
    wait_for_path(&invocation_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-bash","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = child.expect_event("interrupt-bash", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completion_events = child.drain_events_until_event("turn-bash", "turn_completed");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the full bash sleep"
    );
    let completed = completion_events.last().expect("turn_completed");
    assert_eq!(completed["status"], "cancelled");
    let tool_completed = completion_events
        .iter()
        .find(|event| event["event"] == "tool_completed" && event["tool"] == "bash")
        .expect("cancelled bash tool_completed");
    assert_eq!(tool_completed["toolCallId"], "mock-tool-1");
    assert_eq!(tool_completed["status"], "cancelled");
    assert_eq!(tool_completed["kind"], "cancelled");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"bash-items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/items/list");
        stdin.flush().expect("flush thread/items/list");
    }
    let items = child.expect_event("bash-items", "thread_items_list");
    let command = items["data"]
        .as_array()
        .expect("thread items")
        .iter()
        .find(|item| item["item"]["id"] == "mock-tool-1")
        .expect("cancelled bash thread item");
    assert_eq!(command["item"]["type"], "commandExecution");
    assert_eq!(command["item"]["status"], "cancelled");
    assert_eq!(command["item"]["kind"], "cancelled");
    assert_eq!(command["item"]["invocationStarted"], "yes");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-after-interrupt","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"respond after interrupt"}}]}}}}"#,
            thread_id
        )
        .expect("write turn after interrupt");
        stdin.flush().expect("flush turn after interrupt");
    }
    child.expect_event("turn-after-interrupt", "turn_started");
    let next_completed = child.expect_event("turn-after-interrupt", "turn_completed");
    assert_eq!(next_completed["status"], "success");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_interrupt_cancels_active_mcp_tool_wait() {
    let _guard = lock_env();
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_slow_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "mode = \"full-auto\"\n\n[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"{}\"\n",
            server.display()
        ),
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-mcp","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#,
            thread_id
        )
        .expect("write MCP turn");
        stdin.flush().expect("flush MCP turn");
    }
    let turn_started = child.expect_event("turn-mcp", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();
    let tool_requested = child.expect_event("turn-mcp", "tool_requested");
    assert_eq!(tool_requested["tool"], "mcp__slow__wait");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-mcp","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = child.expect_event("interrupt-mcp", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = child.expect_event("turn-mcp", "turn_completed");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the full MCP tool sleep"
    );
    assert_eq!(completed["status"], "cancelled");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_interrupt_cancels_pending_mcp_elicitation() {
    let _guard = lock_env();
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_eliciting_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "mode = \"full-auto\"\n\n[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"{}\"\nstartup_timeout_ms = 5000\ntool_timeout_ms = 5000\n",
            server.display()
        ),
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-mcp-elicit","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mock_stream_tool_delay_ms 0 mcp__slow__wait"}}]}}}}"#,
            thread_id
        )
        .expect("write MCP elicitation turn");
        stdin.flush().expect("flush MCP elicitation turn");
    }
    let turn_started = child.expect_event("turn-mcp-elicit", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();
    let request = child.expect_event("turn-mcp-elicit", "mcp_elicitation_request");
    assert_eq!(request["turnId"], turn_id);
    assert!(
        request["requestId"]
            .as_str()
            .is_some_and(|id| id.contains(&turn_id)),
        "server MCP elicitation request id should be turn-scoped: {request}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-mcp-elicit","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = child.expect_event("interrupt-mcp-elicit", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = child.expect_event("turn-mcp-elicit", "turn_completed");
    child.close_stdin();
    let output = wait_for_child_output_with_timeout(child, Duration::from_millis(1200))
        .expect("server should exit after interrupting pending MCP elicitation");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the MCP elicitation response"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(completed["status"], "cancelled");
    assert_eq!(output.status.code(), Some(0));
}

#[cfg(unix)]
#[test]
fn server_mode_mcp_tool_uses_configured_transport_timeout() {
    let _guard = lock_env();
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_slow_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "mode = \"full-auto\"\n\n[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"{}\"]\nstartup_timeout_ms = 5000\ntool_timeout_ms = 100\n",
            server.display(),
            workspace.path().join("mcp-timeout.log").display()
        ),
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-timeout","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-timeout", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-mcp-timeout","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#,
            thread_id
        )
        .expect("write MCP turn");
        stdin.flush().expect("flush MCP turn");
    }

    let started = Instant::now();
    let events = child.drain_events_until_event("turn-mcp-timeout", "turn_completed");
    assert!(
        started.elapsed() < Duration::from_millis(4000),
        "MCP timeout path waited too long: {:?}",
        started.elapsed()
    );
    let completed = events.last().expect("turn_completed");
    assert_eq!(completed["status"], "success");
    let tool_completed = events
        .iter()
        .find(|event| event["event"] == "tool_completed")
        .expect("tool_completed event");
    assert_eq!(tool_completed["status"], "failed");
    assert!(
        tool_completed["error"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "tool_completed error did not include transport timeout: {tool_completed}"
    );
    let mcp_item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["type"] == "mcpToolCall")
        .expect("mcp tool item_completed");
    assert_eq!(mcp_item_completed["item"]["id"], "mock-tool-1");
    assert_eq!(mcp_item_completed["item"]["server"], "slow");
    assert_eq!(mcp_item_completed["item"]["tool"], "wait");
    assert_eq!(mcp_item_completed["item"]["status"], "failed");
    assert!(
        mcp_item_completed["item"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "mcp item error did not include transport timeout: {mcp_item_completed}"
    );
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"mcp-items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/items/list");
        stdin.flush().expect("flush thread/items/list");
    }
    let persisted_items = child.expect_event("mcp-items", "thread_items_list");
    let persisted_items_data = persisted_items["data"].as_array().expect("persisted items");
    let persisted_mcp_item = persisted_items_data
        .iter()
        .find(|item| {
            item["item"]["type"] == "mcpToolCall"
                && item["item"]["server"] == "slow"
                && item["item"]["tool"] == "wait"
                && item["item"]["status"] == "failed"
                && item["item"]["error"]["message"]
                    .as_str()
                    .is_some_and(|error| {
                        error.contains("MCP request 'tools/call' timed out after 100ms")
                    })
        })
        .unwrap_or_else(|| panic!("persisted mcp timeout item missing: {persisted_items_data:?}"));
    assert_eq!(persisted_mcp_item["item"]["server"], "slow");
    assert_eq!(persisted_mcp_item["item"]["tool"], "wait");
    assert_eq!(persisted_mcp_item["item"]["status"], "failed");
    assert!(
        persisted_mcp_item["item"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "persisted mcp item error did not include transport timeout: {persisted_mcp_item}"
    );
    assert_eq!(
        persisted_mcp_item["item"]["arguments"],
        serde_json::json!({})
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resumes_active_thread_turn_before_cancellation_checkpoint() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"resume active turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = child.expect_event("turn-slow", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-active","method":"turn/interrupt","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/interrupt");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-active-duplicate","method":"turn/interrupt","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write duplicate turn/interrupt");
        writeln!(
            stdin,
            r#"{{"id":"resume-active","method":"turn/resume","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/resume");
        writeln!(
            stdin,
            r#"{{"id":"resume-active-duplicate","method":"turn/resume","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write duplicate turn/resume");
        stdin.flush().expect("flush turn controls");
    }

    let interrupt = child.expect_event("interrupt-active", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let duplicate_interrupt = child.expect_event("interrupt-active-duplicate", "turn_controlled");
    assert_eq!(duplicate_interrupt["status"], "interrupted");
    let resume = child.expect_event("resume-active", "turn_controlled");
    assert_eq!(resume["status"], "resumed");
    let duplicate_resume = child.expect_event("resume-active-duplicate", "turn_controlled");
    assert_eq!(duplicate_resume["status"], "resumed");

    let completed = child.expect_event("turn-slow", "turn_completed");
    assert_eq!(
        completed["status"], "success",
        "resumed turn failed: {completed}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let terminal_events = parse_jsonl(&output.stdout)
        .into_iter()
        .filter(|event| event["id"] == "turn-slow" && event["event"] == "turn_completed")
        .collect::<Vec<_>>();
    assert_eq!(terminal_events.len(), 1);
}

#[test]
fn server_mode_steers_active_thread_turn_as_user_item() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow steerable turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = child.expect_event("turn-slow", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"steer-active","method":"turn/steer","params":{{"threadId":"{}","turnId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/steer");
        stdin.flush().expect("flush turn/steer");
    }

    let controlled = child.expect_event("steer-active", "turn_controlled");
    assert_eq!(controlled["action"], "steer");
    assert_eq!(controlled["turnId"], turn_id);
    assert_eq!(controlled["status"], "steered");
    assert_eq!(controlled["input"], "mock_history_echo");

    let remaining = child.drain_events_until_event("turn-slow", "turn_completed");
    let item_started = remaining
        .iter()
        .find(|event| event["id"] == "steer-active" && event["event"] == "item_started")
        .expect("active steer should emit a user item event");
    assert_eq!(item_started["threadId"], thread_id);
    assert_eq!(item_started["turnId"], turn_id);
    assert_eq!(item_started["item"]["type"], "user_message");
    assert_eq!(item_started["item"]["role"], "user");
    assert_eq!(item_started["item"]["content"], "mock_history_echo");

    let completed = remaining
        .iter()
        .find(|event| event["id"] == "turn-slow" && event["event"] == "turn_completed")
        .expect("turn completion event");
    assert_eq!(completed["status"], "success");
    let message_text = remaining
        .iter()
        .filter(|event| event["id"] == "turn-slow" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        message_text.contains("slow steerable turn | mock_history_echo"),
        "active steer should be visible to the running model context, got: {message_text}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_steers_active_thread_turn_with_multi_text_input() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow steerable turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = child.expect_event("turn-slow", "turn_started");
    let turn_id = turn_started["turnId"]
        .as_str()
        .expect("logical turn id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"steer-active","method":"turn/steer","params":{{"threadId":"{}","turnId":"{}","input":[{{"type":"text","text":"mock_history_echo"}},{{"type":"text","text":"second steer"}}]}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/steer");
        stdin.flush().expect("flush turn/steer");
    }

    let controlled = child.expect_event("steer-active", "turn_controlled");
    assert_eq!(controlled["status"], "steered");
    assert_eq!(controlled["input"], "mock_history_echo\nsecond steer");

    let remaining = child.drain_events_until_event("turn-slow", "turn_completed");
    let item_started = remaining
        .iter()
        .find(|event| event["id"] == "steer-active" && event["event"] == "item_started")
        .expect("active steer should emit a user item event");
    assert_eq!(
        item_started["item"]["content"],
        "mock_history_echo\nsecond steer"
    );

    let message_text = remaining
        .iter()
        .filter(|event| event["id"] == "turn-slow" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        message_text.contains("slow steerable turn | mock_history_echo\nsecond steer"),
        "multi-text steer input should be visible to the running model context, got: {message_text}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_completed_turn_controls() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-done","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"finish quickly"}}]}}}}"#,
            thread_id
        )
        .expect("write completed turn");
        stdin.flush().expect("flush completed turn");
    }
    let completed = child.expect_event("turn-done", "turn_completed");
    assert_eq!(completed["status"], "success");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turns-after-done","method":"thread/turns/list","params":{{"threadId":"{}","limit":1}}}}"#,
            thread_id
        )
        .expect("write turns list");
        stdin.flush().expect("flush turns list");
    }
    let turns = child.expect_event("turns-after-done", "thread_turns_list");
    let completed_turn_id = turns["data"][0]["turnId"]
        .as_str()
        .expect("completed turn id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-completed","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            completed_turn_id
        )
        .expect("write completed turn interrupt");
        writeln!(
            stdin,
            r#"{{"id":"steer-completed","method":"turn/steer","params":{{"turnId":"{}","input":[{{"type":"text","text":"too late"}}]}}}}"#,
            completed_turn_id
        )
        .expect("write completed turn steer");
        stdin.flush().expect("flush completed turn controls");
    }

    child.close_stdin();
    let interrupt = child.expect_event("interrupt-completed", "error");
    assert_eq!(
        interrupt["message"],
        format!("turn is not active: {completed_turn_id}")
    );
    let steer = child.expect_event("steer-completed", "error");
    assert_eq!(
        steer["message"],
        format!("turn is not active: {completed_turn_id}")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_active_turn_id_matches_persisted_turn_id() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-one","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"persisted turn id contract"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let turn_started = child.expect_event("turn-one", "turn_started");
    let active_turn_id = turn_started["turnId"]
        .as_str()
        .expect("active turn id")
        .to_string();
    let runtime_task_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("runtime task id");
    assert_ne!(active_turn_id, runtime_task_id);
    child.expect_event("turn-one", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{}","limit":1}}}}"#,
            thread_id
        )
        .expect("write turns list");
    }
    child.close_stdin();

    let turns = child.expect_event("turns", "thread_turns_list");
    let persisted_turn_id = turns["data"][0]["turnId"]
        .as_str()
        .expect("persisted turn id");
    assert_eq!(active_turn_id, persisted_turn_id);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_turn_control_thread_mismatch() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-a","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread a start");
        writeln!(
            stdin,
            r#"{{"id":"thread-b","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread b start");
        stdin.flush().expect("flush thread starts");
    }
    let thread_a = child.expect_event("thread-a", "thread_started");
    let thread_b = child.expect_event("thread-b", "thread_started");
    let thread_a_id = thread_a["threadId"].as_str().expect("thread a id");
    let thread_b_id = thread_b["threadId"].as_str().expect("thread b id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-a","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow on a"}}]}}}}"#,
            thread_a_id
        )
        .expect("write thread a turn");
        stdin.flush().expect("flush thread a turn");
    }
    let turn_started = child.expect_event("turn-a", "turn_started");
    let turn_id = turn_started["turnId"].as_str().expect("logical turn id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"interrupt-mismatch","method":"turn/interrupt","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_b_id, turn_id
        )
        .expect("write mismatched interrupt");
        stdin.flush().expect("flush mismatched interrupt");
    }

    let error = child.expect_event("interrupt-mismatch", "error");
    assert_eq!(
        error["message"],
        format!("turn {turn_id} does not belong to thread {thread_b_id}")
    );

    let completed = child.expect_event("turn-a", "turn_completed");
    assert_eq!(completed["status"], "success");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_controls_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script(
            r#"read line; printf 'server:%s\n' "$line""#,
            "& $env:COMSPEC /D /V:ON /S /C 'set /p line=& echo server:!line!'",
        );
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "interactive server shell"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }

    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["status"], "running");
    assert_eq!(started["requestedTerminalMode"], "pipe");
    assert_eq!(started["effectiveTerminalMode"], "pipe");
    assert_eq!(
        started["command"],
        platform_shell_script(
            r#"read line; printf 'server:%s\n' "$line""#,
            "& $env:COMSPEC /D /V:ON /S /C 'set /p line=& echo server:!line!'"
        )
    );
    assert!(started["taskId"].as_str().is_some_and(|id| !id.is_empty()));

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-write","method":"shell/write","params":{{"shellId":"{}","input":"from-server\n"}}}}"#,
            shell_id
        )
        .expect("write shell/write");
        stdin.flush().expect("flush shell/write");
    }
    let written = child.expect_event("shell-write", "shell_updated");
    assert_eq!(written["shellId"], shell_id);
    assert_eq!(written["status"], "running");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-close","method":"shell/close","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/close");
        stdin.flush().expect("flush shell/close");
    }
    let closed = child.expect_event("shell-close", "shell_updated");
    assert_eq!(closed["status"], "stdin_closed");

    let mut read_events = Vec::new();
    let read_deadline = Instant::now() + Duration::from_secs(5);
    for attempt in 0.. {
        let request_id = format!("shell-read-{attempt}");
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"{}","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                request_id, shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let events = read_events_until_shell_read_response(&mut child, &request_id);
        let completed = events
            .iter()
            .any(|event| event["event"] == "shell_completed");
        read_events.extend(events);
        if completed {
            break;
        }
        assert!(
            Instant::now() < read_deadline,
            "shell/read did not observe shell completion before deadline; events={read_events:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.close_stdin();
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_output_delta"),
        "shell/read should stream output delta before completion"
    );
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_exited"),
        "shell/read should stream shell_exited before legacy completion"
    );
    let completed = read_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell_completed event");
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0, "{completed:?}");
    assert_eq!(
        completed["stdout"],
        platform_shell_script("server:from-server\n", "server:from-server\r\n")
    );
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_returns_buffered_output() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command(
                    "printf 'legacy-out'; printf 'legacy-err' >&2",
                    "process.stdout.write('legacy-out'); process.stderr.write('legacy-err')"
                ),
                "tty": false,
                "streamStdin": false,
                "streamStdoutStderr": false
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0, "{completed:?}");
    assert_eq!(completed["stdout"], "legacy-out");
    assert_eq!(completed["stderr"], "legacy-err");
}

#[cfg(windows)]
#[test]
fn server_mode_command_exec_preserves_native_windows_argv() {
    let workspace = tempdir().expect("workspace");
    let expected_args = vec![
        "".to_string(),
        "two words".to_string(),
        "single'quote".to_string(),
        "double\"quote".to_string(),
        "&|<>^%!".to_string(),
        "line one\nline two".to_string(),
        r"路径\文件.txt".to_string(),
    ];
    let mut command = vec![
        "node".to_string(),
        "-e".to_string(),
        "process.stdout.write(JSON.stringify(process.argv.slice(1)))".to_string(),
    ];
    command.extend(expected_args.iter().cloned());
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {"command": command}
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0, "{completed:?}");
    assert_eq!(
        completed["stdout"],
        serde_json::to_string(&expected_args).expect("expected argv json")
    );
    assert_eq!(completed["stderr"], "");
}

#[test]
fn server_mode_command_exec_preserves_legacy_script_command() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_shell_script(
                    "printf legacy-script",
                    "Write-Host -NoNewline legacy-script"
                )
            }
        });
        writeln!(stdin, "{request}").expect("write legacy command/exec");
        stdin.flush().expect("flush legacy command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "legacy-script");
    assert_eq!(completed["stderr"], "");
}

#[test]
fn server_mode_command_exec_honors_cwd_and_env_overrides() {
    let workspace = tempdir().expect("workspace");
    let command_dir = workspace.path().join("command-dir");
    std::fs::create_dir(&command_dir).expect("create command cwd");
    let command_dir = std::fs::canonicalize(command_dir).expect("canonical command cwd");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_COMMAND_EXEC_BASE", "server")
        .env("ORCA_COMMAND_EXEC_REMOVE", "server")
        .sandbox_workspace(&command_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command(
                    "printf '%s|%s|%s|%s' \"$PWD\" \"$ORCA_COMMAND_EXEC_BASE\" \"$ORCA_COMMAND_EXEC_EXTRA\" \"${ORCA_COMMAND_EXEC_REMOVE-unset}\"",
                    "const removed = process.env.ORCA_COMMAND_EXEC_REMOVE || 'unset'; process.stdout.write([process.cwd(), process.env.ORCA_COMMAND_EXEC_BASE, process.env.ORCA_COMMAND_EXEC_EXTRA, removed].join('|'))"
                ),
                "cwd": command_dir,
                "env": {
                    "ORCA_COMMAND_EXEC_BASE": "request",
                    "ORCA_COMMAND_EXEC_EXTRA": "added",
                    "ORCA_COMMAND_EXEC_REMOVE": null
                },
                "tty": false,
                "streamStdin": false,
                "streamStdoutStderr": false
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        completed["stdout"],
        format!("{}|request|added|unset", command_dir.display())
    );
    assert_eq!(completed["stderr"], "");
}

#[test]
fn server_mode_command_exec_uses_thread_additional_working_directories() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("orca-command-additional-roots-");
    let home_path = parent.path().join("home");
    {
        let workspace = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        std::fs::create_dir_all(&home_path).expect("orca home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&extra).expect("extra");
        let output_file = extra.join("allowed.txt");
        let command = format!("printf allowed > {}", output_file.display());

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().unwrap(),
            ])
            .env("ORCA_HOME", &home_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread_started = child.expect_event("thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"grant","method":"turn/start","params":{{"threadId":"{}","permissionUpdates":[{{"type":"addDirectories","destination":"session","directories":["{}"]}}],"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id,
                extra.display()
            )
            .expect("write permission grant turn");
            stdin.flush().expect("flush permission grant turn");
        }
        child.expect_event("grant", "turn_completed");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request(
                "cmd",
                &command,
                "throw 'unexpected Windows execution'",
                json!({"threadId": thread_id}),
            );
            writeln!(stdin, "{request}").expect("write command/exec");
            stdin.flush().expect("flush command/exec");
        }
        let completed = child.expect_event("cmd", "command_exec_completed");
        child.close_stdin();
        assert_eq!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("allowed output"),
            "allowed"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    }
}

#[cfg(not(windows))]
#[test]
fn server_mode_command_exec_uses_session_network_domain_grants() {
    let home = tempdir().expect("orca home");
    let home_path = home.path();
    let workspace_root = tempdir().expect("workspace");
    let workspace = workspace_root.path().to_path_buf();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test server");
    let port = listener.local_addr().expect("server addr").port();
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out waiting for proxied retry",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 18\r\n\r\nsession-network-ok")?;
        Ok(())
    });
    std::fs::write(
        home_path.join("config.toml"),
        "mode = \"full-auto\"\n\n[permission_profiles.net]\nextends = \":workspace\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"seed.orca.invalid\" = \"allow\"\n",
    )
    .expect("write config");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .env("ORCA_HOME", home_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request(
            "cmd-request",
            &format!("curl --noproxy '' --max-time 2 -sS http://127.0.0.1:{port}/ || true"),
            "exit 0",
            json!({"threadId": thread_id, "permissionProfile": "net", "timeoutMs": 5000}),
        );
        writeln!(stdin, "{request}").expect("write command/exec permission request");
        stdin
            .flush()
            .expect("flush command/exec permission request");
    }

    let permission_request =
        child.expect_event("permission-command-cmd-request", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();
    assert_eq!(permission_request["threadId"], thread_id);
    assert_eq!(
        permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
        "allow"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":null,"network":{{"enabled":true,"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#,
            request_id,
        )
        .expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }
    child.expect_event("permission-response", "permission_resolved");
    server
        .join()
        .expect("local test server joined")
        .expect("local test server completed");
    let retried = child.expect_event("cmd-request", "command_exec_completed");
    assert_eq!(retried["exitCode"], 0);
    assert_eq!(retried["stdout"], "session-network-ok");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
        stdin.flush().expect("flush thread/read");
    }
    let read = child.expect_event("read", "thread_read");
    assert_eq!(read["networkDomainPermissionCount"], 1);
    assert_eq!(read["networkDomainPermissions"]["127.0.0.1"], "allow");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
        stdin.flush().expect("flush thread/list");
    }
    let list = child.expect_event("list", "thread_list");
    let listed = list["data"]
        .as_array()
        .expect("thread list data")
        .iter()
        .find(|thread| thread["threadId"] == thread_id)
        .expect("listed thread");
    assert_eq!(listed["networkDomainPermissionCount"], 1);
    assert_eq!(listed["networkDomainPermissions"]["127.0.0.1"], "allow");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request(
            "cmd",
            "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
            "exit 0",
            json!({"threadId": thread_id, "timeoutMs": 5000}),
        );
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let blocked_request = child.expect_event("permission-command-cmd", "permission_request");
    assert_eq!(
        blocked_request["permissions"]["network"]["domains"]["blocked.orca.invalid"],
        "allow"
    );
    let blocked_request_id = blocked_request["requestId"]
        .as_str()
        .expect("blocked permission request id")
        .to_string();
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"permission-deny","method":"permission/respond","params":{{"requestId":"{}","decision":"deny","scope":"turn","permissions":{{"fileSystem":null,"network":{{"enabled":true,"domains":{{"blocked.orca.invalid":"allow"}}}}}}}}}}"#,
            blocked_request_id,
        )
        .expect("write permission/respond deny");
        stdin.flush().expect("flush permission/respond deny");
    }
    child.expect_event("permission-deny", "permission_resolved");
    let error = child.expect_event("cmd", "error");
    child.close_stdin();
    assert_eq!(
        error["message"],
        format!("command/exec permission denied: {blocked_request_id}")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(not(windows))]
#[test]
fn server_mode_session_network_deny_overrides_permission_profile_allow() {
    let home = tempdir().expect("orca home");
    let home_path = home.path();
    let workspace_root = tempdir().expect("workspace");
    let workspace = workspace_root.path().to_path_buf();
    std::fs::write(
        home_path.join("config.toml"),
        "mode = \"full-auto\"\n\n[permission_profiles.requester]\nextends = \":workspace\"\n\n[permission_profiles.requester.network]\nenabled = true\n\n[permission_profiles.requester.network.domains]\n\"seed.orca.invalid\" = \"allow\"\n\n[permission_profiles.net]\nextends = \":workspace\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"blocked.orca.invalid\" = \"allow\"\n",
    )
    .expect("write config");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .env("ORCA_HOME", home_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request(
            "cmd-request",
            "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
            "exit 0",
            json!({"threadId": thread_id, "permissionProfile": "requester", "timeoutMs": 5000}),
        );
        writeln!(stdin, "{request}").expect("write command/exec permission request");
        stdin
            .flush()
            .expect("flush command/exec permission request");
    }
    let permission_request =
        child.expect_event("permission-command-cmd-request", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();
    assert_eq!(permission_request["threadId"], thread_id);
    assert_eq!(
        permission_request["permissions"]["network"]["domains"]["blocked.orca.invalid"],
        "allow"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":null,"network":{{"enabled":true,"domains":{{"blocked.orca.invalid":"deny"}}}}}}}}}}"#,
            request_id,
        )
        .expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }
    child.expect_event("permission-response", "permission_resolved");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request(
            "cmd",
            "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
            "exit 0",
            json!({"threadId": thread_id, "permissionProfile": "net", "timeoutMs": 5000}),
        );
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let error = child.expect_event("cmd", "error");
    child.close_stdin();
    assert_eq!(
        error["message"],
        "command/exec network access to blocked.orca.invalid was denied by configured network policy"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_danger_full_access_bypasses_workspace_sandbox() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("orca-command-sandbox-");
    let workspace = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&outside).expect("outside");
    let blocked_file = outside.join("blocked.txt");
    let allowed_file = outside.join("allowed.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&blocked_file, "blocked");
        let request = command_exec_request_for_platform_script("blocked", &command, json!({}));
        writeln!(stdin, "{request}").expect("write sandboxed command/exec");
        stdin.flush().expect("flush sandboxed command/exec");
    }
    let blocked = child.expect_event("blocked", "command_exec_completed");
    assert_ne!(blocked["exitCode"], 0);
    assert!(!blocked_file.exists());

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&allowed_file, "allowed");
        let request = command_exec_request_for_platform_script(
            "allowed",
            &command,
            json!({"sandboxPolicy": {"type": "dangerFullAccess"}}),
        );
        writeln!(stdin, "{request}").expect("write danger full access command/exec");
        stdin
            .flush()
            .expect("flush danger full access command/exec");
    }
    child.close_stdin();

    let allowed = child.expect_event("allowed", "command_exec_completed");
    assert_eq!(allowed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&allowed_file).expect("allowed output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_allows_only_writable_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = sandbox_test_parent("orca-command-workspace-write-");
    let workspace = parent.path().join("workspace");
    let allowed_root = parent.path().join("allowed");
    let blocked_root = parent.path().join("blocked");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&allowed_root).expect("allowed root");
    std::fs::create_dir(&blocked_root).expect("blocked root");
    let allowed_file = allowed_root.join("allowed.txt");
    let blocked_file = blocked_root.join("blocked.txt");
    let command = format!(
        "{}; {}",
        platform_write_file_command(&allowed_file, "allowed"),
        platform_write_file_command(&blocked_file, "blocked")
    );

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [allowed_root], "networkAccess": true, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}}),
        );
        writeln!(stdin, "{request}").expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&allowed_file).expect("allowed output"),
        "allowed"
    );
    assert!(!blocked_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_read_only_blocks_workspace_writes() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = platform_write_file_command(&workspace_file, "blocked");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"sandboxPolicy": {"type": "readOnly", "networkAccess": false}}),
        );
        writeln!(stdin, "{request}").expect("write readOnly command/exec");
        stdin.flush().expect("flush readOnly command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_permission_profile_read_only_blocks_workspace_writes() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = platform_write_file_command(&workspace_file, "blocked");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"permissionProfile": "read-only"}),
        );
        writeln!(stdin, "{request}").expect("write read-only permissionProfile command/exec");
        stdin
            .flush()
            .expect("flush read-only permissionProfile command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_inherits_thread_active_permission_profile() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = format!("printf blocked > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down","extends":":read-only"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    child.expect_event("turn", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-c",{}]}}}}"#,
            thread_id,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write inherited permissionProfile command/exec");
        stdin
            .flush()
            .expect("flush inherited permissionProfile command/exec");
    }
    child.close_stdin();
    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_resolves_thread_active_permission_profile_from_config() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.locked-down]\nextends = \":read-only\"\n",
        )
        .expect("write permission profile config");

        let workspace = tempdir().expect("workspace");
        let workspace_file = workspace.path().join("blocked.txt");
        let command = format!("printf blocked > {}", workspace_file.display());

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = child.expect_event("thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        child.expect_event("turn", "turn_completed");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-c",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write config-backed permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush config-backed permissionProfile command/exec");
        }
        child.close_stdin();
        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_uses_configured_permission_profile_filesystem_write_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let extra = tempdir().expect("extra");
        let workspace_file = workspace.path().join("blocked.txt");
        let extra_file = extra.path().join("allowed.txt");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.extra-write]\nextends = \":read-only\"\n\n[permission_profiles.extra-write.filesystem]\n\"{}\" = \"write\"\n",
                extra.path().display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            extra_file.display(),
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = child.expect_event("thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"extra-write"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        child.expect_event("turn", "turn_completed");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"threadId": thread_id}),
            );
            writeln!(stdin, "{request}").expect("write filesystem permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush filesystem permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&extra_file).expect("extra output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_workspace_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let runtime_root = tempdir().expect("runtime root");
        let docs = runtime_root.path().join("docs");
        std::fs::create_dir(&docs).expect("create docs");
        let docs_file = docs.join("allowed.txt");
        let workspace_file = workspace.path().join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.docs]\nextends = \":read-only\"\n\n[permission_profiles.docs.filesystem]\n\":workspace_roots/docs\" = \"write\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            docs_file.display(),
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{"runtimeWorkspaceRoots":["{}"]}}}}"#,
                runtime_root.path().display()
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = child.expect_event("thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"docs"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        child.expect_event("turn", "turn_completed");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"threadId": thread_id}),
            );
            writeln!(stdin, "{request}")
                .expect("write workspace roots permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush workspace roots permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&docs_file).expect("docs output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_uses_scoped_filesystem_entries() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let runtime_root = tempdir().expect("runtime root");
        let docs = runtime_root.path().join("docs");
        let secrets = runtime_root.path().join("secrets");
        std::fs::create_dir(&docs).expect("create docs");
        std::fs::create_dir(&secrets).expect("create secrets");
        let docs_file = docs.join("allowed.txt");
        let secret_file = secrets.join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.docs]\nextends = \":read-only\"\n\n[permission_profiles.docs.filesystem.\":workspace_roots\"]\ndocs = \"write\"\nsecrets = \"deny\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            docs_file.display(),
            secret_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{"runtimeWorkspaceRoots":["{}"]}}}}"#,
                runtime_root.path().display()
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = child.expect_event("thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"docs"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        child.expect_event("turn", "turn_completed");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"threadId": thread_id}),
            );
            writeln!(stdin, "{request}")
                .expect("write scoped filesystem permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush scoped filesystem permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&docs_file).expect("docs output"),
            "allowed"
        );
        assert!(!secret_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_uses_trailing_globstar_subtree() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let allowed_file = allowed.path().join("nested").join("allowed.txt");
        let blocked_file = workspace.path().join("blocked.txt");
        std::fs::create_dir_all(allowed_file.parent().expect("allowed parent"))
            .expect("create allowed parent");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.globstar]\nextends = \":read-only\"\n\n[permission_profiles.globstar.filesystem]\n\"{}/**\" = \"write\"\n",
                allowed.path().display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            shell_escape(&allowed_file),
            shell_escape(&blocked_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"permissionProfile": "globstar"}),
            );
            writeln!(stdin, "{request}").expect("write globstar permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush globstar permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&allowed_file).expect("allowed output"),
            "allowed"
        );
        assert!(!blocked_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_deny_overrides_write_root() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let denied = allowed.path().join("denied");
        std::fs::create_dir(&denied).expect("denied dir");
        let allowed_file = allowed.path().join("allowed.txt");
        let denied_file = denied.join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.mixed]\nextends = \":read-only\"\n\n[permission_profiles.mixed.filesystem]\n\"{}\" = \"write\"\n\"{}\" = \"deny\"\n",
                allowed.path().display(),
                denied.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            allowed_file.display(),
            denied_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"permissionProfile": "mixed"}),
            );
            writeln!(stdin, "{request}").expect("write deny permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush deny permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&allowed_file).expect("allowed output"),
            "allowed"
        );
        assert!(!denied_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_deny_blocks_reads() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let denied = allowed.path().join("denied");
        std::fs::create_dir(&denied).expect("denied dir");
        let secret_file = denied.join("secret.txt");
        let leaked_file = allowed.path().join("leaked.txt");
        std::fs::write(&secret_file, "secret").expect("write secret");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.mixed]\nextends = \":read-only\"\n\n[permission_profiles.mixed.filesystem]\n\"{}\" = \"write\"\n\"{}\" = \"deny\"\n",
                allowed.path().display(),
                denied.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "set -e; secret=$(cat {}); printf %s \"$secret\" > {}",
            shell_escape(&secret_file),
            shell_escape(&leaked_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"permissionProfile": "mixed"}),
            );
            writeln!(stdin, "{request}").expect("write deny-read permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush deny-read permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert!(!leaked_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_enforces_deny_glob_entries() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let parent = sandbox_test_parent("orca-command-deny-read-");
        let workspace = parent.path().join("workspace");
        let allowed = parent.path().join("allowed");
        std::fs::create_dir(&workspace).expect("workspace dir");
        std::fs::create_dir(&allowed).expect("allowed dir");
        let denied_file = allowed.join("secret.env");
        let ordinary_file = allowed.join("ordinary.txt");
        let output_file = allowed.join("ordinary.out");
        std::fs::write(&denied_file, "secret").expect("write denied file");
        std::fs::write(&ordinary_file, "ordinary").expect("write ordinary file");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.globbed]\nextends = \":read-only\"\n\n[permission_profiles.globbed.filesystem]\n\"{}\" = \"read-write\"\n\"{}/*.env\" = \"deny\"\n",
                allowed.display(),
                allowed.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "set -e; cat {} > {}; cat {}",
            shell_escape(&ordinary_file),
            shell_escape(&output_file),
            shell_escape(&denied_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"permissionProfile": "globbed"}),
            );
            writeln!(stdin, "{request}").expect("write glob permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush glob permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("ordinary output"),
            "ordinary"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_minimal_special_path() {
    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.minimal]\nextends = \":read-only\"\n\n[permission_profiles.minimal.filesystem]\n\":minimal\" = \"read\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request(
                "cmd",
                "true",
                "exit 0",
                json!({"permissionProfile": "minimal"}),
            );
            writeln!(stdin, "{request}").expect("write minimal permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush minimal permissionProfile command/exec");
        }
        child.close_stdin();

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());

        let events = parse_jsonl(&output.stdout);
        assert!(
            events
                .iter()
                .any(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed"),
            "expected command completion for :minimal profile: {events:?}"
        );
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_enforces_network_domain_policy() {
    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.net]\nextends = \":workspace\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"blocked.orca.invalid\" = \"deny\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = unix_command_exec_request(
                "cmd",
                "curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true",
                json!({"permissionProfile": "net", "timeoutMs": 5000}),
            );
            writeln!(stdin, "{request}")
                .expect("write network domain permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush network domain permissionProfile command/exec");
        }
        child.close_stdin();

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());

        let events = parse_jsonl(&output.stdout);
        assert_eq!(events.len(), 1, "expected one command event: {events:?}");
        assert_eq!(events[0]["id"], "cmd");
        assert_eq!(events[0]["event"], "error");
        #[cfg(windows)]
        assert_eq!(
            events[0]["message"],
            "Windows domain-restricted network sandbox is unavailable; refusing to run without an OS-enforced network boundary"
        );
        #[cfg(not(windows))]
        assert_eq!(
            events[0]["message"],
            "command/exec network access to blocked.orca.invalid was denied by configured network policy"
        );
    });
}

#[cfg(not(windows))]
#[test]
fn server_mode_bash_inherits_thread_active_permission_profile_network_policy() {
    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "mode = \"full-auto\"\n\n[permission_profiles.net]\nextends = \":workspace\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"api.example.com\" = \"allow\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = child.expect_event("thread", "thread_started");
        let thread_id = thread_started["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"net"}},"input":[{{"type":"text","text":"bash curl --max-time 2 --proxy \"$HTTP_PROXY\" -sS -D - -o /dev/null http://blocked.orca.invalid/ || true"}}]}}}}"#,
                thread_id
            )
            .expect("write bash turn");
            stdin.flush().expect("flush bash turn");
        }

        let permission_request = child.expect_event("turn", "permission_request");
        assert_eq!(permission_request["threadId"], thread_id);
        assert!(
            permission_request["reason"]
                .as_str()
                .expect("permission reason")
                .contains("blocked.orca.invalid"),
            "permission request should identify blocked host: {permission_request:?}"
        );
        assert_eq!(
            permission_request["permissions"]["network"]["domains"]["blocked.orca.invalid"],
            "allow"
        );
        let request_id = permission_request["requestId"]
            .as_str()
            .expect("permission request id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"deny","scope":"turn","permissions":{{"fileSystem":null,"network":{{"domains":{{"blocked.orca.invalid":"allow"}}}}}}}}}}"#,
                request_id
            )
            .expect("write permission/respond");
            stdin.flush().expect("flush permission/respond");
        }
        let resolved = child.expect_event("permission-response", "permission_resolved");
        assert_eq!(resolved["requestId"], request_id);
        assert_eq!(resolved["decision"], "deny");
        let _completed = child.expect_event("turn", "turn_completed");

        child.close_stdin();
        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[cfg(not(windows))]
#[test]
fn server_mode_bash_network_permission_allow_retries_with_grant() {
    with_orca_home(|home| {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local test server");
        let port = listener.local_addr().expect("server addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 15\r\n\r\nbash-network-ok")
                .expect("write response");
        });
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "mode = \"full-auto\"\n\n[permission_profiles.net]\nextends = \":workspace\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"api.example.com\" = \"allow\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = child.expect_event("thread", "thread_started");
        let thread_id = thread_started["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"net"}},"input":[{{"type":"text","text":"bash curl --max-time 2 --proxy \"$HTTP_PROXY\" -sS http://127.0.0.1:{}/"}}]}}}}"#,
                thread_id,
                port
            )
            .expect("write bash turn");
            stdin.flush().expect("flush bash turn");
        }

        let permission_request = child.expect_event("turn", "permission_request");
        assert_eq!(permission_request["threadId"], thread_id);
        assert_eq!(
            permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
            "allow"
        );
        let request_id = permission_request["requestId"]
            .as_str()
            .expect("permission request id");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"turn","permissions":{{"fileSystem":null,"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#,
                request_id
            )
            .expect("write permission/respond");
            stdin.flush().expect("flush permission/respond");
        }
        let resolved = child.expect_event("permission-response", "permission_resolved");
        assert_eq!(resolved["requestId"], request_id);
        assert_eq!(resolved["decision"], "allow");
        server.join().expect("server joined");

        let completed =
            child.expect_event_matching("turn", "tool_completed", |event| event["tool"] == "bash");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output"], "bash-network-ok");
        let _turn_completed = child.expect_event("turn", "turn_completed");

        child.close_stdin();
        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_tmpdir() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let tmpdir = tempdir().expect("tmpdir");
        let tmp_file = tmpdir.path().join("allowed.txt");
        let workspace_file = workspace.path().join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.tmp]\nextends = \":read-only\"\n\n[permission_profiles.tmp.filesystem]\n\":tmpdir\" = \"write\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > \"$TMPDIR/allowed.txt\"; printf blocked > {}",
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .env("TMPDIR", tmpdir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = command_exec_request_for_platform_script(
                "cmd",
                &command,
                json!({"permissionProfile": "tmp"}),
            );
            writeln!(stdin, "{request}").expect("write tmpdir permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush tmpdir permissionProfile command/exec");
        }
        child.close_stdin();

        let completed = child.expect_event("cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&tmp_file).expect("tmp output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_sandbox_policy_overrides_thread_active_permission_profile() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("allowed.txt");
    let command = format!("printf allowed > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down","extends":":read-only"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    child.expect_event("turn", "turn_completed");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"threadId": thread_id, "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": true, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}}),
        );
        writeln!(stdin, "{request}").expect("write explicit sandboxPolicy command/exec");
        stdin
            .flush()
            .expect("flush explicit sandboxPolicy command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&workspace_file).expect("workspace output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_external_sandbox_bypasses_workspace_sandbox() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("allowed.txt");
    let command = format!("printf allowed > {}", outside_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"sandboxPolicy": {"type": "externalSandbox", "networkAccess": "enabled"}}),
        );
        writeln!(stdin, "{request}").expect("write externalSandbox command/exec");
        stdin.flush().expect("flush externalSandbox command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("outside output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_can_exclude_slash_tmp() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let tmp_file = std::env::temp_dir().join(format!(
        "orca-command-exclude-slash-tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let command = format!("printf blocked > {}", tmp_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": true, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true}}),
        );
        writeln!(stdin, "{request}").expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!tmp_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_allows_slash_tmp_by_default() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let tmp_file = std::env::temp_dir().join(format!(
        "orca-command-allow-slash-tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let command = format!("printf allowed > {}", tmp_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = command_exec_request_for_platform_script(
            "cmd",
            &command,
            json!({"sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": true, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}}),
        );
        writeln!(stdin, "{request}").expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    child.close_stdin();

    let completed = child.expect_event("cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&tmp_file).expect("tmp output"),
        "allowed"
    );
    let _ = std::fs::remove_file(tmp_file);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_respects_buffered_output_cap() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command(
                    "printf 'abcdef'; printf 'uvwxyz' >&2",
                    "process.stdout.write('abcdef'); process.stderr.write('uvwxyz')"
                ),
                "outputBytesCap": 5
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "abcde");
    assert_eq!(completed["stderr"], "uvwxy");
}

#[test]
fn server_mode_command_exec_caps_buffered_output_by_bytes() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command(
                    "printf 'ééé'; printf 'ééé' >&2",
                    "process.stdout.write('ééé'); process.stderr.write('ééé')"
                ),
                "outputBytesCap": 5
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "éé");
    assert_eq!(completed["stderr"], "éé");
}

#[test]
fn server_mode_command_exec_rejects_invalid_option_combinations() {
    assert_command_exec_error(
        json!({"command": platform_command("sleep 1", "Start-Sleep -Seconds 1"), "processId": "invalid-timeout", "disableTimeout": true, "timeoutMs": 1000}),
        "command/exec cannot set both timeoutMs and disableTimeout",
    );
    assert_command_exec_error(
        json!({"command": platform_command("sleep 1", "Start-Sleep -Seconds 1"), "processId": "invalid-cap", "disableOutputCap": true, "outputBytesCap": 1024}),
        "command/exec cannot set both outputBytesCap and disableOutputCap",
    );
    assert_command_exec_error(
        json!({"command": platform_command("sleep 1", "Start-Sleep -Seconds 1"), "processId": "negative-timeout", "timeoutMs": -1}),
        "command/exec timeoutMs must be non-negative, got -1",
    );
    assert_command_exec_error(
        json!({"command": platform_command("true", "exit 0"), "sandboxPolicy": {"type": "dangerFullAccess"}, "permissionProfile": "read-only"}),
        "`permissionProfile` cannot be combined with `sandboxPolicy`",
    );
    assert_command_exec_error(
        json!({"command": platform_command("cat", "$input | Write-Output"), "streamStdoutStderr": true}),
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        json!({"command": platform_command("cat", "$input | Write-Output"), "streamStdin": true}),
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        json!({"command": platform_command("printf tty", "Write-Host -NoNewline 'tty'"), "tty": true}),
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        json!({"command": platform_command("true", "exit 0"), "processId": "size-without-tty", "size": {"rows": 24, "cols": 80}}),
        "command/exec size requires tty: true",
    );
    assert_command_exec_error(
        json!({"command": platform_command("true", "exit 0"), "processId": "zero-size", "tty": true, "size": {"rows": 0, "cols": 80}}),
        "command/exec size rows and cols must be greater than 0",
    );
}

#[test]
fn server_mode_command_exec_with_process_id_can_be_terminated() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("command-started");
    let release_marker = workspace.path().join("command-release");
    let started_marker_arg = started_marker.to_str().expect("marker path");
    let release_marker_arg = release_marker.to_str().expect("release marker path");
    let windows_command = format!(
        "const fs = require('fs'); process.stdout.write('started'); fs.writeFileSync({}, ''); const poll = setInterval(() => {{ if (fs.existsSync({})) {{ clearInterval(poll); process.stdout.write('done'); }} }}, 50);",
        javascript_path(&started_marker),
        javascript_path(&release_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command_with_args(
                    "printf started; : > \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.05; done; printf done",
                    &windows_command,
                    &[started_marker_arg, release_marker_arg]
                ),
                "processId": "sleep-1",
                "tty": false,
                "streamStdin": false,
                "streamStdoutStderr": false
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "sleep-1");
    wait_for_path(&started_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"sleep-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }

    let terminated = child.expect_event("cmd-kill", "command_exec_terminated");
    assert_eq!(terminated["processId"], "sleep-1");

    child.close_stdin();
    let events = child.drain_events_until_event("cmd", "command_exec_completed");
    let completed = events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_ne!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_stops_active_processes_when_input_closes() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("command-started");
    let release_marker = workspace.path().join("command-release");
    let leaked_marker = workspace.path().join("command-still-running");
    let started_marker_arg = started_marker.to_str().expect("started marker path");
    let release_marker_arg = release_marker.to_str().expect("release marker path");
    let leaked_marker_arg = leaked_marker.to_str().expect("marker path");
    let windows_command = format!(
        "const fs = require('fs'); process.stdout.write('started'); fs.writeFileSync({}, ''); const poll = setInterval(() => {{ if (fs.existsSync({})) {{ clearInterval(poll); fs.writeFileSync({}, 'leaked'); }} }}, 50);",
        javascript_path(&started_marker),
        javascript_path(&release_marker),
        javascript_path(&leaked_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command_with_args(
                    "printf started; : > \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.05; done; printf leaked > \"$3\"",
                    &windows_command,
                    &[started_marker_arg, release_marker_arg, leaked_marker_arg]
                ),
                "processId": "eof-cleanup-1",
                "streamStdoutStderr": true
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "eof-cleanup-1");
    wait_for_path(&started_marker);

    child.close_stdin();
    let output =
        wait_for_child_output_with_timeout(child, Duration::from_secs(3)).expect("server exited");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    std::fs::write(&release_marker, "release").expect("write release marker");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !leaked_marker.exists(),
        "active command/exec process should be stopped when server input closes"
    );
}

#[cfg(unix)]
#[test]
fn server_mode_shell_stops_active_process_group_when_input_closes() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("shell-started");
    let release_marker = workspace.path().join("shell-release");
    let leaked_marker = workspace.path().join("shell-still-running");
    let shell_quote = |path: &Path| {
        let value = path.to_string_lossy().replace('\'', r#"'"'"'"#);
        format!("'{value}'")
    };
    let command = format!(
        "printf started > {}; (while [ ! -e {} ]; do sleep 0.05; done; printf leaked > {}) & wait",
        shell_quote(&started_marker),
        shell_quote(&release_marker),
        shell_quote(&leaked_marker),
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "id": "shell-start",
                "method": "shell/start",
                "params": {
                    "command": command,
                    "description": "EOF cleanup server shell",
                }
            })
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    child.expect_event("shell-start", "shell_started");
    wait_for_path(&started_marker);

    child.close_stdin();
    let output =
        wait_for_child_output_with_timeout(child, Duration::from_secs(3)).expect("server exited");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    std::fs::write(&release_marker, "release").expect("write release marker");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !leaked_marker.exists(),
        "active shell process group should be stopped when server input closes"
    );
}

#[test]
fn server_mode_command_exec_rejects_duplicate_active_process_id() {
    let workspace = tempdir().expect("workspace");
    let duplicate_marker = workspace.path().join("duplicate-started");
    let duplicate_marker_arg = duplicate_marker.to_str().expect("marker path");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd-1",
            "method": "command/exec",
            "params": {
                "command": platform_command("sleep 30", "Start-Sleep -Seconds 30"),
                "processId": "dup-1"
            }
        });
        writeln!(stdin, "{request}").expect("write first command/exec");
        stdin.flush().expect("flush first command/exec");
    }
    let started = child.expect_event("cmd-1", "command_exec_started");
    assert_eq!(started["processId"], "dup-1");

    {
        let stdin = child.stdin_mut();
        let duplicate_command = platform_fixture_command_with_args(
            "printf leaked > \"$1\"",
            &format!(
                "require('fs').writeFileSync({}, 'leaked')",
                javascript_path(&duplicate_marker)
            ),
            &[duplicate_marker_arg],
        );
        let request = json!({
            "id": "cmd-2",
            "method": "command/exec",
            "params": {
                "command": duplicate_command,
                "processId": "dup-1"
            }
        });
        writeln!(stdin, "{request}").expect("write duplicate command/exec");
        stdin.flush().expect("flush duplicate command/exec");
    }

    let duplicate = child.expect_next_for_id("cmd-2");
    assert_eq!(duplicate["event"], "error");
    assert_eq!(
        duplicate["message"],
        "duplicate active command/exec process id: \"dup-1\""
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"dup-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    child.expect_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    child.drain_events_until_event("cmd-1", "command_exec_completed");
    assert!(
        !duplicate_marker.exists(),
        "duplicate command/exec process id should be rejected before spawning a process"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_list_returns_active_process_snapshots() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("command-started");
    let release_marker = workspace.path().join("command-release");
    let completed_marker = workspace.path().join("command-completed");
    let started_marker_arg = started_marker.to_str().expect("marker path");
    let release_marker_arg = release_marker.to_str().expect("release marker path");
    let completed_marker_arg = completed_marker.to_str().expect("completed marker path");
    let command = "printf listed; touch $1; while test ! -e $2; do sleep 0.05; done; touch $3";
    let windows_command = format!(
        "const fs = require('fs'); process.stdout.write('listed'); fs.writeFileSync({}, ''); const poll = setInterval(() => {{ if (fs.existsSync({})) {{ clearInterval(poll); fs.writeFileSync({}, ''); }} }}, 50);",
        javascript_path(&started_marker),
        javascript_path(&release_marker),
        javascript_path(&completed_marker)
    );
    let command_args = platform_fixture_command_with_args(
        command,
        &windows_command,
        &[started_marker_arg, release_marker_arg, completed_marker_arg],
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = serde_json::json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": command_args,
                "processId": "listed-1",
                "cwd": workspace.path().to_str().unwrap(),
                "streamStdoutStderr": true,
                "outputBytesCap": 32,
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "listed-1");
    wait_for_path(&started_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-list","method":"command/exec/list","params":{{}}}}"#
        )
        .expect("write command/exec/list");
        stdin.flush().expect("flush command/exec/list");
    }

    let list_events = child.drain_events_until_event_or_error("cmd-list", "command_exec_listed");
    let listed = list_events
        .iter()
        .find(|event| event["id"] == "cmd-list")
        .expect("command/exec/list response");
    assert_eq!(listed["event"], "command_exec_listed");
    let processes = listed["processes"]
        .as_array()
        .expect("command exec process list");
    assert_eq!(processes.len(), 1);
    let process = &processes[0];
    assert_eq!(process["processId"], "listed-1");
    assert!(
        process["shellId"].as_str().is_some(),
        "command/exec/list should expose the backing shell session id: {process}"
    );
    assert!(
        process["taskId"].as_str().is_some(),
        "command/exec/list should expose the backing shell task id: {process}"
    );
    assert_eq!(process["status"], "running");
    assert_eq!(process["requestedTerminalMode"], "pipe");
    assert_eq!(process["effectiveTerminalMode"], "pipe");
    assert_eq!(process["cwd"], workspace.path().to_str().unwrap());
    assert_eq!(process["streamOutput"], true);
    assert_eq!(process["outputBytesCap"], 32);
    assert_eq!(process["stdoutBytes"], 6);
    assert_eq!(process["stderrBytes"], 0);
    assert_eq!(process["command"], serde_json::json!(command_args));

    std::fs::write(&release_marker, "release").expect("write release marker");
    wait_for_path(&completed_marker);
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-list-after","method":"command/exec/list","params":{{}}}}"#
        )
        .expect("write second command/exec/list");
        stdin.flush().expect("flush second command/exec/list");
    }
    let after_release_events =
        child.drain_events_until_event_or_error("cmd-list-after", "command_exec_listed");
    assert!(
        after_release_events
            .iter()
            .any(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed"),
        "completed command/exec process should be drained before the next list: {after_release_events:?}"
    );
    let after_release_listed = after_release_events
        .iter()
        .find(|event| event["id"] == "cmd-list-after")
        .expect("second command/exec/list response");
    assert_eq!(after_release_listed["event"], "command_exec_listed");
    assert_eq!(
        after_release_listed["processes"]
            .as_array()
            .expect("process list after completion")
            .len(),
        0
    );
    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_write_requires_input_or_close() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command("cat", "process.stdin.resume()"),
                "processId": "write-empty-1",
                "streamStdin": true
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "write-empty-1");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"write-empty-1"}}}}"#
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    let error = child.expect_next_for_id("cmd-write");
    assert_eq!(error["event"], "error");
    assert_eq!(
        error["message"],
        "command/exec/write requires deltaBase64 or closeStdin"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"write-empty-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    child.expect_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    child.drain_events_until_event("cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_resize_rejects_zero_dimensions() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command("cat", "process.stdin.resume()"),
                "processId": "resize-zero-1",
                "tty": true,
                "size": {"rows": 24, "cols": 80}
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "resize-zero-1");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-resize","method":"command/exec/resize","params":{{"processId":"resize-zero-1","size":{{"rows":0,"cols":80}}}}}}"#
        )
        .expect("write command/exec/resize");
        stdin.flush().expect("flush command/exec/resize");
    }

    let error = child.expect_next_for_id("cmd-resize");
    assert_eq!(error["event"], "error");
    assert_eq!(
        error["message"],
        "command/exec size rows and cols must be greater than 0"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"resize-zero-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    child.expect_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    child.drain_events_until_event("cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_streams_output_and_accepts_write() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command(
                    "printf 'out-start\n'; printf 'err-start\n' >&2; IFS= read line; printf 'out:%s\n' \"$line\"; printf 'err:%s\n' \"$line\" >&2",
                    "process.stdout.write('out-start\\n'); process.stderr.write('err-start\\n'); process.stdin.setEncoding('utf8'); let input = ''; process.stdin.on('data', chunk => input += chunk); process.stdin.on('end', () => { const line = input.replace(/\\r?\\n$/, ''); process.stdout.write(`out:${line}\\n`); process.stderr.write(`err:${line}\\n`); })"
                ),
                "processId": "pipe-1",
                "streamStdin": true,
                "streamStdoutStderr": true
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "pipe-1");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"pipe-1","deltaBase64":"{}","closeStdin":true}}}}"#,
            STANDARD.encode("hello\n")
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    let mut completion_events = child.drain_events_until_event("cmd-write", "command_exec_written");
    let write_ack = completion_events
        .last()
        .expect("command_exec_written event");
    assert_eq!(write_ack["processId"], "pipe-1");
    completion_events.extend(child.drain_events_until_event("cmd", "command_exec_completed"));
    assert_command_exec_delta_seen(&completion_events, "stdout", "out-start");
    assert_command_exec_delta_seen(&completion_events, "stderr", "err-start");
    assert_command_exec_delta_seen(&completion_events, "stdout", "out:hello");
    assert_command_exec_delta_seen(&completion_events, "stderr", "err:hello");
    let completed = completion_events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_read_drains_streaming_output() {
    let workspace = tempdir().expect("workspace");
    let release_marker = workspace.path().join("command-release");
    let release_marker_arg = release_marker.to_str().expect("release marker path");
    let windows_command = format!(
        "while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; Write-Host -NoNewline 'read-out'; Write-Error 'read-err'; Start-Sleep -Seconds 30",
        powershell_path(&release_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_command_with_args(
                    "while [ ! -e \"$1\" ]; do sleep 0.05; done; printf 'read-out'; printf 'read-err' >&2; sleep 30",
                    &windows_command,
                    &[release_marker_arg]
                ),
                "processId": "read-1",
                "streamStdoutStderr": true
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "read-1");
    std::fs::write(&release_marker, "release").expect("write release marker");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-read","method":"command/exec/read","params":{{"processId":"read-1","timeoutMs":5000}}}}"#
        )
        .expect("write command/exec/read");
        stdin.flush().expect("flush command/exec/read");
    }

    let mut read_events = child.drain_events_until_event_or_error("cmd-read", "command_exec_read");
    let read_ack = read_events
        .iter()
        .find(|event| event["id"] == "cmd-read" && event["event"] == "command_exec_read")
        .expect("command_exec_read event");
    assert_eq!(read_ack["processId"], "read-1");
    assert_eq!(read_ack["status"], "running");
    let saw_stdout = command_exec_events_contain(&read_events, "stdout", "read-out");
    let saw_stderr = command_exec_events_contain(&read_events, "stderr", "read-err");
    if !saw_stdout || !saw_stderr {
        read_events.extend(read_command_exec_output_until(
            &mut child,
            "read-1",
            |stdout, stderr| {
                (saw_stdout || stdout.contains("read-out"))
                    && (saw_stderr || stderr.contains("read-err"))
            },
        ));
    }
    assert_command_exec_delta_seen(&read_events, "stdout", "read-out");
    assert_command_exec_delta_seen(&read_events, "stderr", "read-err");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"read-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    child.expect_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    child.drain_events_until_event("cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_read_caps_streaming_output() {
    let workspace = tempdir().expect("workspace");
    let release_marker = workspace.path().join("command-cap-release");
    let release_marker_arg = release_marker.to_str().expect("release marker path");
    let windows_command = format!(
        "while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; Write-Host -NoNewline 'abcdef'; Start-Sleep -Seconds 30",
        powershell_path(&release_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_command_with_args(
                    "while [ ! -e \"$1\" ]; do sleep 0.05; done; printf 'abcdef'; sleep 30",
                    &windows_command,
                    &[release_marker_arg]
                ),
                "processId": "read-cap-1",
                "streamStdoutStderr": true
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "read-cap-1");
    std::thread::sleep(Duration::from_millis(1200));
    std::fs::write(&release_marker, "release").expect("write release marker");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-read","method":"command/exec/read","params":{{"processId":"read-cap-1","timeoutMs":5000,"outputBytesCap":3}}}}"#
        )
        .expect("write command/exec/read");
        stdin.flush().expect("flush command/exec/read");
    }

    let mut read_events = child.drain_events_until_event_or_error("cmd-read", "command_exec_read");
    let read_ack = read_events
        .iter()
        .find(|event| event["id"] == "cmd-read" && event["event"] == "command_exec_read")
        .expect("command_exec_read event");
    assert_eq!(read_ack["processId"], "read-cap-1");
    assert_eq!(read_ack["status"], "running");
    if !command_exec_events_contain(&read_events, "stdout", "abc") {
        read_events.extend(read_command_exec_output_until(
            &mut child,
            "read-cap-1",
            |stdout, _stderr| stdout.contains("abc"),
        ));
    }
    assert_command_exec_delta_seen(&read_events, "stdout", "abc");
    assert!(
        read_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == "stdout"
                && event["capReached"] == true
        }),
        "missing capReached stdout delta: {read_events:?}"
    );
    assert!(
        !read_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains('d'))
        }),
        "read output exceeded cap: {read_events:?}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"read-cap-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    child.expect_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    child.drain_events_until_event("cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_streaming_respects_output_cap() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("stream-cap-started");
    let started_marker_arg = started_marker.to_str().expect("started marker path");
    let windows_command = format!(
        "const fs = require('fs'); process.stdout.write('abcdefghij'); fs.writeFileSync({}, ''); setTimeout(() => {{}}, 30_000);",
        javascript_path(&started_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command_with_args(
                    "printf 'abcdefghij'; : > \"$1\"; sleep 30",
                    &windows_command,
                    &[started_marker_arg]
                ),
                "processId": "stream-cap-1",
                "streamStdoutStderr": true,
                "outputBytesCap": 5
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "stream-cap-1");
    wait_for_path(&started_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"stream-cap-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    let mut events = child.drain_events_until_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    events.extend(child.drain_events_until_event("cmd", "command_exec_completed"));
    assert_command_exec_delta_seen(&events, "stdout", "abcde");
    assert_command_exec_output_delta_notification_seen(&events, "stdout", "stream-cap-1");
    assert!(
        events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == "stdout"
                && event["capReached"] == true
        }),
        "missing capReached stdout delta: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains("f"))
        }),
        "streaming output exceeded cap: {events:?}"
    );
    let completed = events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_ne!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_caps_streaming_output_by_bytes() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("stream-byte-cap-started");
    let started_marker_arg = started_marker.to_str().expect("started marker path");
    let windows_command = format!(
        "const fs = require('fs'); process.stdout.write('ééé'); fs.writeFileSync({}, ''); setTimeout(() => {{}}, 30_000);",
        javascript_path(&started_marker)
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "cmd",
            "method": "command/exec",
            "params": {
                "command": platform_fixture_command_with_args(
                    "printf 'ééé'; : > \"$1\"; sleep 30",
                    &windows_command,
                    &[started_marker_arg]
                ),
                "processId": "stream-byte-cap-1",
                "streamStdoutStderr": true,
                "outputBytesCap": 5
            }
        });
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "stream-byte-cap-1");
    wait_for_path(&started_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"stream-byte-cap-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    let mut events = child.drain_events_until_event("cmd-kill", "command_exec_terminated");
    child.close_stdin();
    events.extend(child.drain_events_until_event("cmd", "command_exec_completed"));
    assert_command_exec_delta_seen(&events, "stdout", "éé");
    assert!(
        events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == "stdout"
                && event["capReached"] == true
        }),
        "missing capReached stdout delta: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains("ééé"))
        }),
        "streaming output exceeded byte cap: {events:?}"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_command_exec_tty_supports_initial_size_and_resize() {
    let workspace = tempdir().expect("workspace");
    let started_marker = workspace.path().join("tty-size-started");
    let started_marker_arg = started_marker.to_str().expect("started marker path");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["python3","-c","import fcntl,termios,struct,sys,pathlib; data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack('HHHH',0,0,0,0)); rows,cols,_,_=struct.unpack('HHHH', data); print(f'start:{{rows}} {{cols}}', flush=True); pathlib.Path(sys.argv[1]).write_text('started'); sys.stdin.readline(); data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack('HHHH',0,0,0,0)); rows,cols,_,_=struct.unpack('HHHH', data); print(f'after:{{rows}} {{cols}}', flush=True)","{started_marker_arg}"],"processId":"tty-size-1","tty":true,"size":{{"rows":31,"cols":101}}}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    let started = child.expect_event("cmd", "command_exec_started");
    assert_eq!(started["processId"], "tty-size-1");
    wait_for_path(&started_marker);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-resize","method":"command/exec/resize","params":{{"processId":"tty-size-1","size":{{"rows":45,"cols":132}}}}}}"#
        )
        .expect("write command/exec/resize");
        stdin.flush().expect("flush command/exec/resize");
    }
    let mut completion_events =
        child.drain_events_until_event("cmd-resize", "command_exec_resized");
    let resize_ack = completion_events
        .last()
        .expect("command_exec_resized event");
    assert_eq!(resize_ack["processId"], "tty-size-1");
    assert_eq!(resize_ack["rows"], 45);
    assert_eq!(resize_ack["cols"], 132);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"tty-size-1","deltaBase64":"{}","closeStdin":true}}}}"#,
            STANDARD.encode("go\n")
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    completion_events.extend(child.drain_events_until_event("cmd-write", "command_exec_written"));
    let write_ack = completion_events
        .iter()
        .rev()
        .find(|event| event["id"] == "cmd-write" && event["event"] == "command_exec_written")
        .expect("command_exec_written event");
    assert_eq!(write_ack["processId"], "tty-size-1");
    completion_events.extend(child.drain_events_until_event("cmd", "command_exec_completed"));
    assert_command_exec_delta_seen(&completion_events, "stdout", "start:31 101");
    assert_command_exec_delta_seen(&completion_events, "stdout", "after:45 132");
    let completed = completion_events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_reports_shell_capabilities() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-caps","method":"shell/capabilities","params":{{}}}}"#
        )
        .expect("write shell/capabilities");
        stdin.flush().expect("flush shell/capabilities");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let caps = events
        .iter()
        .find(|event| event["id"] == "shell-caps" && event["event"] == "shell_capabilities")
        .expect("shell_capabilities event");

    assert!(
        caps["platform"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        caps["supportedTerminalModes"],
        serde_json::json!(["pipe", "pty"])
    );
    assert_eq!(caps["fallbackTerminalMode"], "pipe");
    assert_eq!(caps["commandExecStreamingRequiresProcessId"], true);

    #[cfg(any(unix, windows))]
    {
        assert_eq!(caps["supportsPty"], true);
        assert_eq!(caps["supportsPtyResize"], true);
    }
    #[cfg(not(any(unix, windows)))]
    {
        assert_eq!(caps["supportsPty"], false);
        assert_eq!(caps["supportsPtyResize"], false);
    }
}

#[test]
fn server_mode_kills_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script(
            "printf started; sleep 30; printf done",
            "Write-Host -NoNewline 'started'; Start-Sleep -Seconds 30; Write-Host -NoNewline 'done'",
        );
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "killable server shell"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["requestedTerminalMode"], "pipe");
    assert_eq!(started["effectiveTerminalMode"], "pipe");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
            shell_id
        )
        .expect("write shell/read");
        stdin.flush().expect("flush shell/read");
    }
    let read_events = child.drain_events_until_event("shell-read", "shell_updated");
    assert!(
        read_events.iter().any(|event| {
            event["event"] == "shell_output_delta"
                && event["stream"] == "stdout"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains("started"))
        }),
        "shell must publish startup output before the kill contract is exercised: {read_events:?}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
    }

    child.close_stdin();
    let killed = child.expect_event("shell-kill", "shell_completed");
    assert_eq!(killed["shellId"], shell_id);
    assert_eq!(killed["status"], "stopped");
    assert!(
        killed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_task_stop_reaps_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-start","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread-start", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id").to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script(
            "printf started; sleep 30; printf done",
            "Write-Output 'started'; Start-Sleep -Seconds 30; Write-Output 'done'",
        );
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {
                "threadId": thread_id,
                "command": command,
                "description": "task-stoppable server shell"
            }
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    let task_id = started["taskId"].as_str().expect("task id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"task-stop-turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"task_stop {}"}}]}}}}"#,
            thread_id,
            task_id
        )
        .expect("write task_stop turn/start");
        stdin.flush().expect("flush task_stop turn/start");
    }
    let turn_events = child.drain_events_until_event("task-stop-turn", "turn_completed");
    let task_stop_completed = turn_events
        .iter()
        .find(|event| event["event"] == "tool_completed" && event["tool"] == "task_stop")
        .expect("task_stop tool_completed");
    assert_eq!(task_stop_completed["status"], "completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
            shell_id
        )
        .expect("write shell/read");
        stdin.flush().expect("flush shell/read");
    }

    child.close_stdin();
    let read_events = child.drain_events_until_event("shell-read", "shell_completed");
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_exited"),
        "shell/read should emit shell_exited after task_stop"
    );
    let completed = read_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell_completed event");
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["taskId"], task_id);
    assert_eq!(completed["status"], "stopped");
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_reads_runtime_shell_session_incrementally() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script(
            "printf ready; sleep 30; printf done",
            "Write-Host -NoNewline 'ready'; Start-Sleep -Seconds 30; Write-Host -NoNewline 'done'",
        );
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "incremental server shell"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
            shell_id
        )
        .expect("write shell/read");
        stdin.flush().expect("flush shell/read");
    }

    let read_events = child.drain_events_until_event("shell-read", "shell_updated");
    let output_delta = read_events
        .iter()
        .find(|event| event["event"] == "shell_output_delta")
        .expect("shell output delta");
    assert_eq!(output_delta["shellId"], shell_id);
    assert_eq!(output_delta["stream"], "stdout");
    assert_eq!(output_delta["delta"], "ready");
    assert_eq!(output_delta["final"], false);

    let update = read_events
        .iter()
        .find(|event| event["event"] == "shell_updated")
        .expect("shell_updated event");
    assert_eq!(update["shellId"], shell_id);
    assert_eq!(update["status"], "running");
    assert_eq!(update["stdout"], "ready");
    assert_eq!(update["stderr"], "");
    assert_eq!(update["exitCode"], Value::Null);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }

    child.close_stdin();
    let kill_events = child.drain_events_until_event("shell-kill", "shell_completed");
    let exited = kill_events
        .iter()
        .find(|event| event["event"] == "shell_exited")
        .expect("shell exited event");
    assert_eq!(exited["shellId"], shell_id);
    assert!(exited["exitCode"].is_number());

    let killed = kill_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell completed event");
    assert_eq!(killed["shellId"], shell_id);
    assert_eq!(killed["status"], "stopped");
    assert!(
        killed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("ready")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_shell_read_honors_output_byte_cap() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script(
            "printf ready-long-output; sleep 30",
            "Write-Output 'ready-long-output'; Start-Sleep -Seconds 30",
        );
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "capped server shell"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000,"outputBytesCap":5}}}}"#,
            shell_id
        )
        .expect("write capped shell/read");
        stdin.flush().expect("flush capped shell/read");
    }

    let read_events = child.drain_events_until_event("shell-read", "shell_updated");
    let output_delta = read_events
        .iter()
        .find(|event| event["event"] == "shell_output_delta")
        .expect("shell output delta");
    assert_eq!(output_delta["shellId"], shell_id);
    assert_eq!(output_delta["stream"], "stdout");
    assert_eq!(output_delta["delta"], "ready");
    assert_eq!(output_delta["capReached"], true);
    assert_eq!(output_delta["final"], false);

    let update = read_events
        .iter()
        .find(|event| event["event"] == "shell_updated")
        .expect("shell_updated event");
    assert_eq!(update["shellId"], shell_id);
    assert_eq!(update["status"], "running");
    assert_eq!(update["stdout"], "ready");
    assert_eq!(update["stderr"], "");
    assert_eq!(update["capReached"], true);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }

    child.close_stdin();
    let _ = child.drain_events_until_event("shell-kill", "shell_completed");
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_runtime_shell_sessions() {
    let workspace = tempdir().expect("workspace");
    let command = platform_shell_script(
        "printf ready; sleep 30",
        "Write-Output 'ready'; Start-Sleep -Seconds 30",
    );
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "listed server shell"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    let task_id = started["taskId"].as_str().expect("task id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-list","method":"shell/list","params":{{}}}}"#
        )
        .expect("write shell/list");
        stdin.flush().expect("flush shell/list");
    }
    let listed = child.expect_event("shell-list", "shell_listed");
    assert_eq!(listed["shells"].as_array().expect("shell list").len(), 1);
    let shell = &listed["shells"][0];
    assert_eq!(shell["shellId"], shell_id);
    assert_eq!(shell["taskId"], task_id);
    assert_eq!(shell["command"], command);
    assert_eq!(shell["status"], "running");
    assert_eq!(shell["requestedTerminalMode"], "pipe");
    assert_eq!(shell["effectiveTerminalMode"], "pipe");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }
    child.expect_event("shell-kill", "shell_completed");
    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_updates_runtime_shell_session_description() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script("sleep 30", "Start-Sleep -Seconds 30");
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "old shell label"}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-update","method":"shell/update","params":{{"shellId":"{}","description":"new shell label"}}}}"#,
            shell_id
        )
        .expect("write shell/update");
        stdin.flush().expect("flush shell/update");
    }
    let updated = child.expect_event("shell-update", "shell_updated");
    assert_eq!(updated["shellId"], shell_id);
    assert_eq!(updated["status"], "updated");
    assert_eq!(updated["description"], "new shell label");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-list","method":"shell/list","params":{{}}}}"#
        )
        .expect("write shell/list");
        stdin.flush().expect("flush shell/list");
    }
    let listed = child.expect_event("shell-list", "shell_listed");
    assert_eq!(listed["shells"][0]["shellId"], shell_id);
    assert_eq!(listed["shells"][0]["description"], "new shell label");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }
    child.expect_event("shell-kill", "shell_completed");
    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_starts_runtime_shell_session_with_pty() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"if test -t 0 && test -t 1; then printf tty; else printf pipe; fi","description":"pty server shell","pty":true}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["requestedTerminalMode"], "pty");
    assert_eq!(started["effectiveTerminalMode"], "pty");

    let completed = loop {
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut child, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    child.close_stdin();
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        completed["stdout"].as_str().unwrap_or_default().trim(),
        "tty"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_resizes_runtime_shell_pty_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"python3 -c 'import fcntl,termios,struct,sys; sys.stdin.readline(); data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack(\"HHHH\",0,0,0,0)); rows,cols,_,_=struct.unpack(\"HHHH\", data); print(f\"{{rows}} {{cols}}\")'","description":"resizable pty shell","pty":true}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-resize","method":"shell/resize","params":{{"shellId":"{}","cols":120,"rows":33}}}}"#,
            shell_id
        )
        .expect("write shell/resize");
        stdin.flush().expect("flush shell/resize");
    }
    let resized = child.expect_next_for_id("shell-resize");
    assert_eq!(resized["event"], "shell_updated");
    assert_eq!(resized["shellId"], shell_id);
    assert_eq!(resized["status"], "resized");
    assert_eq!(resized["cols"], 120);
    assert_eq!(resized["rows"], 33);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-write","method":"shell/write","params":{{"shellId":"{}","input":"\n"}}}}"#,
            shell_id
        )
        .expect("write shell/write");
        stdin.flush().expect("flush shell/write");
    }
    let _ = child.expect_event("shell-write", "shell_updated");

    let completed = loop {
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut child, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    child.close_stdin();
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("33 120"),
        "resized PTY should report 33 rows and 120 cols, got: {}",
        completed["stdout"]
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_starts_runtime_shell_pty_session_with_initial_size() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"python3 -c 'import fcntl,termios,struct,sys; data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack(\"HHHH\",0,0,0,0)); rows,cols,_,_=struct.unpack(\"HHHH\", data); print(f\"{{rows}} {{cols}}\")'","description":"sized pty shell","terminalMode":"pty","cols":132,"rows":41}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    let completed = loop {
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut child, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    child.close_stdin();
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("41 132"),
        "initial PTY size should report 41 rows and 132 cols, got: {}",
        completed["stdout"]
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_resize_for_pipe_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script("sleep 30", "Start-Sleep -Seconds 30");
        let request = json!({
            "id": "shell-start",
            "method": "shell/start",
            "params": {"command": command, "description": "pipe shell", "pty": false}
        });
        writeln!(stdin, "{request}").expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = child.expect_event("shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-resize","method":"shell/resize","params":{{"shellId":"{}","cols":120,"rows":33}}}}"#,
            shell_id
        )
        .expect("write shell/resize");
        stdin.flush().expect("flush shell/resize");
    }
    let resized = child.expect_next_for_id("shell-resize");
    assert_eq!(resized["event"], "error");
    assert!(
        resized["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not a PTY")
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }

    child.close_stdin();
    let killed = child.expect_event("shell-kill", "shell_completed");
    assert_eq!(killed["status"], "stopped");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_routes_turn_start_to_started_thread() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = child.expect_event("thread-req", "thread_started");
    assert_eq!(thread_started["id"], "thread-req");
    assert_eq!(thread_started["event"], "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();
    assert!(!thread_id.is_empty());

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-req","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"hello bound thread"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start request");
    }

    let turn_events = child.drain_events_until_event("turn-req", "turn_completed");
    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    assert!(
        turn_events
            .iter()
            .any(|event| event["event"] == "turn_started")
    );
    assert!(
        turn_events
            .iter()
            .any(|event| event["event"] == "turn_completed")
    );
}

#[test]
fn server_mode_preserves_thread_conversation_across_turns() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#,
            thread_id
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }

    child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write second turn");
    }

    let turn_events = child.drain_events_until_event("turn-2", "turn_completed");
    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let echoed = turn_events
        .iter()
        .find_map(|event| {
            (event["event"] == "message_delta")
                .then(|| event["text"].as_str().map(ToString::to_string))
                .flatten()
        })
        .unwrap_or_default();

    assert!(
        echoed.contains("first prompt | mock_history_echo"),
        "expected second turn to see prior thread history, got: {echoed}"
    );
}

#[test]
fn server_mode_atomic_file_mention_uses_the_bound_workspace_root() {
    let first = tempdir().expect("first root");
    let second = tempdir().expect("second root");
    std::fs::write(first.path().join("same.txt"), "content-from-first-root").expect("first file");
    std::fs::write(second.path().join("same.txt"), "content-from-second-root")
        .expect("second file");
    let second_root = second.path().canonicalize().expect("canonical second root");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "thread-req",
                "method": "thread/start",
                "params": {
                    "runtimeWorkspaceRoots": [first.path(), second.path()]
                }
            })
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-1",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [
                        {"type": "text", "text": "inspect "},
                        {
                            "type": "mention",
                            "name": "same.txt",
                            "target": {
                                "type": "file",
                                "root": second_root,
                                "path": "same.txt",
                                "kind": "file"
                            }
                        }
                    ]
                }
            })
        )
        .expect("write atomic mention turn");
        stdin.flush().expect("flush atomic mention turn");
    }
    child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-2",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            })
        )
        .expect("write history echo turn");
        stdin.flush().expect("flush history echo turn");
    }
    let events = child.drain_events_until_event("turn-2", "turn_completed");
    let echoed = events
        .iter()
        .filter(|event| event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();

    assert!(
        echoed.contains("content-from-second-root"),
        "bound second-root file should enter model history: {echoed}"
    );
    assert!(
        !echoed.contains("content-from-first-root"),
        "same relative path from first root must not be expanded: {echoed}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_plain_at_file_remains_literal() {
    let root = tempdir().expect("workspace root");
    std::fs::write(root.path().join("same.txt"), "must-not-be-injected").expect("workspace file");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "thread-req",
                "method": "thread/start",
                "params": {
                    "runtimeWorkspaceRoots": [root.path()]
                }
            })
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-1",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "inspect @same.txt and @oai/sky还能逆向吗"}]
                }
            })
        )
        .expect("write literal at-token turn");
        stdin.flush().expect("flush literal at-token turn");
    }
    child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-2",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            })
        )
        .expect("write history echo turn");
        stdin.flush().expect("flush history echo turn");
    }
    let events = child.drain_events_until_event("turn-2", "turn_completed");
    let echoed = events
        .iter()
        .filter(|event| event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();

    assert!(
        echoed.contains("inspect @same.txt and @oai/sky还能逆向吗"),
        "plain at-token should remain literal in model history: {echoed}"
    );
    assert!(
        !echoed.contains("must-not-be-injected"),
        "plain at-token must not inject matching file content: {echoed}"
    );
    assert!(
        !echoed.contains("<file"),
        "plain at-token must not create a file context block: {echoed}"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_stale_structured_mention_releases_thread_for_next_turn() {
    let root = tempdir().expect("workspace root");
    let root_path = root
        .path()
        .canonicalize()
        .expect("canonical workspace root");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "thread-req",
                "method": "thread/start",
                "params": {"runtimeWorkspaceRoots": [root_path]}
            })
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-stale",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{
                        "type": "mention",
                        "name": "missing.txt",
                        "target": {
                            "type": "file",
                            "root": root_path,
                            "path": "missing.txt",
                            "kind": "file"
                        }
                    }]
                }
            })
        )
        .expect("write stale mention turn");
        stdin.flush().expect("flush stale mention turn");
    }
    let rejected = child.expect_event("turn-stale", "error");
    assert!(
        rejected["message"]
            .as_str()
            .is_some_and(|message| message.contains("failed to resolve bound @missing.txt")),
        "unexpected rejection: {rejected}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "turn-after-rejection",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "thread is still usable"}]
                }
            })
        )
        .expect("write turn after rejection");
        stdin.flush().expect("flush turn after rejection");
    }
    child.expect_event("turn-after-rejection", "turn_completed");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_updates_thread_metadata_and_reads_title() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"rename","method":"thread/metadata/update","params":{{"threadId":"{}","title":"CLI renamed thread"}}}}"#,
            thread_id
        )
        .expect("write metadata update");
        stdin.flush().expect("flush metadata update");
    }

    let renamed = child.expect_event("rename", "thread_metadata_updated");
    assert_eq!(renamed["threadId"], thread_id);
    assert_eq!(renamed["title"], "CLI renamed thread");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id
        )
        .expect("write thread/read request");
    }

    child.close_stdin();
    let read = child.expect_event("read", "thread_read");
    assert_eq!(read["threadId"], thread_id);
    assert_eq!(read["title"], "CLI renamed thread");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_started_thread_from_session_store() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    child.close_stdin();

    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    assert!(
        listed_threads
            .iter()
            .any(|thread| thread["threadId"] == thread_id),
        "thread/list did not include server-started thread {thread_id}: {listed}"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_persists_started_thread_permission_profile() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    child.close_stdin();

    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    let thread = listed_threads
        .iter()
        .find(|thread| thread["threadId"] == thread_id)
        .expect("listed server thread");
    assert_eq!(thread["approvalMode"], "plan");
    assert_eq!(thread["permissionRuleCount"], 1);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resume_and_fork_inherit_thread_permission_profile() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("write current config");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = child.expect_next_for_id("resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = child.expect_next_for_id("fork");
    assert_eq!(forked["event"], "thread_started");
    let forked_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    child.close_stdin();

    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    for thread_id in [&resumed_id, &forked_id] {
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == *thread_id)
            .expect("listed resumed/forked thread");
        assert_eq!(thread["approvalMode"], "plan");
        assert_eq!(thread["permissionRuleCount"], 1);
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resume_and_fork_apply_explicit_permission_override() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("write current config");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}","approvalMode":"auto-edit","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}}}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = child.expect_next_for_id("resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}","approvalMode":"auto-edit","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}}}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = child.expect_next_for_id("fork");
    assert_eq!(forked["event"], "thread_started");
    let forked_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    child.close_stdin();

    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    for thread_id in [&resumed_id, &forked_id] {
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == *thread_id)
            .expect("listed resumed/forked thread");
        assert_eq!(thread["approvalMode"], "auto-edit");
        assert_eq!(thread["permissionRuleCount"], 1);
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_applies_approval_policy_override() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","approvalPolicy":"never","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start request");
        stdin.flush().expect("flush turn/start request");
    }
    let _turn_completed = child.expect_event("turn", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    child.close_stdin();

    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    let thread = listed_threads
        .iter()
        .find(|thread| thread["threadId"] == thread_id)
        .expect("listed thread");
    assert_eq!(thread["approvalMode"], "full-auto");
    assert_eq!(thread["permissionRuleCount"], 1);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_applies_package3_permission_updates() {
    with_orca_home(|home| {
        std::fs::write(
            home.join("config.toml"),
            "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"rm -rf *\"\ndecision = \"deny\"\n[[permissions.rules]]\ntool = \"write_file\"\npattern = \"/tmp/**\"\ndecision = \"prompt\"\n",
        )
        .expect("write original config");
        let extra_dir = home.join("extra");
        let removed_dir = home.join("removed");
        std::fs::create_dir_all(&extra_dir).expect("extra dir");
        std::fs::create_dir_all(&removed_dir).expect("removed dir");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = child.expect_event("thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin_mut();
            let request = json!({
                "id": "turn",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "activePermissionProfile": {"id": "locked-down", "extends": ":workspace"},
                    "permissionUpdates": [
                        {"type": "setMode", "mode": "bypassPermissions", "destination": "session"},
                        {"type": "removeRules", "behavior": "allow", "destination": "session", "rules": [{"toolName": "Bash", "ruleContent": "cargo *"}]},
                        {"type": "addRules", "behavior": "allow", "destination": "session", "rules": [{"toolName": "Bash", "ruleContent": "cargo test *"}]},
                        {"type": "replaceRules", "behavior": "ask", "destination": "session", "rules": [{"toolName": "Write", "ruleContent": "/workspace/**"}]},
                        {"type": "addDirectories", "destination": "session", "directories": [extra_dir]},
                        {"type": "removeDirectories", "destination": "session", "directories": [removed_dir]}
                    ],
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            });
            writeln!(stdin, "{request}").expect("write turn/start request");
            stdin.flush().expect("flush turn/start request");
        }
        let _turn_completed = child.expect_event("turn", "turn_completed");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
            )
            .expect("write thread/list");
        }

        let listed = child.expect_event("list", "thread_list");
        let listed_threads = listed["data"].as_array().expect("thread list data");
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == thread_id)
            .expect("listed thread");
        assert_eq!(thread["approvalMode"], "full-auto");
        assert_eq!(thread["activePermissionProfile"]["id"], "locked-down");
        assert_eq!(thread["activePermissionProfile"]["extends"], ":workspace");
        assert_eq!(thread["permissionRuleCount"], 3);
        assert_eq!(thread["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            thread["additionalWorkingDirectories"][0]["path"],
            extra_dir.display().to_string()
        );
        assert_eq!(
            thread["additionalWorkingDirectories"][0]["source"],
            "session"
        );

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read");
        }
        child.close_stdin();
        let read = child.expect_event("read", "thread_read");
        assert_eq!(read["activePermissionProfile"]["id"], "locked-down");
        assert_eq!(read["activePermissionProfile"]["extends"], ":workspace");
        assert_eq!(read["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read["additionalWorkingDirectories"][0]["path"],
            extra_dir.display().to_string()
        );
        assert_eq!(read["additionalWorkingDirectories"][0]["source"], "session");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load persisted thread");
        assert_eq!(persisted.meta.permission_rules.rules[0].pattern, "rm -rf *");
        assert_eq!(
            persisted.meta.permission_rules.rules[1].pattern,
            "cargo test *"
        );
        assert_eq!(
            persisted.meta.permission_rules.rules[2].pattern,
            "/workspace/**"
        );
        let active_profile = persisted
            .meta
            .active_permission_profile
            .expect("active profile");
        assert_eq!(active_profile.id, "locked-down");
        assert_eq!(active_profile.extends.as_deref(), Some(":workspace"));
        assert_eq!(persisted.meta.additional_working_directories.len(), 1);
        assert_eq!(
            persisted.meta.additional_working_directories[0].path,
            extra_dir
        );
        assert_eq!(
            persisted.meta.additional_working_directories[0].source,
            "session"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_permission_updates_remove_directories_by_destination() {
    with_orca_home(|home| {
        std::fs::create_dir_all(home).expect("create ORCA_HOME");
        std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
        let shared_dir = home.join("shared");
        std::fs::create_dir_all(&shared_dir).expect("shared dir");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = child.expect_event("thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin_mut();
            let request = json!({
                "id": "turn-add",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "permissionUpdates": [
                        {"type": "addDirectories", "destination": "projectSettings", "directories": [shared_dir]},
                        {"type": "addDirectories", "destination": "session", "directories": [shared_dir]}
                    ],
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            });
            writeln!(stdin, "{request}").expect("write add directories turn");
            stdin.flush().expect("flush add directories turn");
        }
        child.expect_event("turn-add", "turn_completed");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"read-after-add","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read after add");
            stdin.flush().expect("flush thread/read after add");
        }
        let read_after_add = child.expect_event("read-after-add", "thread_read");
        assert_eq!(read_after_add["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read_after_add["additionalWorkingDirectories"][0]["path"],
            shared_dir.display().to_string()
        );
        assert_eq!(
            read_after_add["additionalWorkingDirectories"][0]["source"],
            "session"
        );

        {
            let stdin = child.stdin_mut();
            let request = json!({
                "id": "turn-remove",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "permissionUpdates": [{
                        "type": "removeDirectories",
                        "destination": "projectSettings",
                        "directories": [shared_dir]
                    }],
                    "input": [{"type": "text", "text": "mock_history_echo"}]
                }
            });
            writeln!(stdin, "{request}").expect("write remove directories turn");
            stdin.flush().expect("flush remove directories turn");
        }
        child.expect_event("turn-remove", "turn_completed");

        {
            let stdin = child.stdin_mut();
            writeln!(
                stdin,
                r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read");
        }
        child.close_stdin();
        let read = child.expect_event("read", "thread_read");
        assert_eq!(read["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read["additionalWorkingDirectories"][0]["path"],
            shared_dir.display().to_string()
        );
        assert_eq!(read["additionalWorkingDirectories"][0]["source"], "session");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load persisted thread");
        assert_eq!(persisted.meta.additional_working_directories.len(), 1);
        assert_eq!(
            persisted.meta.additional_working_directories[0].path,
            shared_dir
        );
        assert_eq!(
            persisted.meta.additional_working_directories[0].source,
            "session"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_request_permissions_waits_for_permission_response() {
    let parent = sandbox_test_parent("orca-request-permissions-turn-");
    let workspace = parent.path().join("workspace");
    let home = parent.path().join("home");
    let extra = parent.path().join("extra");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let output_file = extra.join("granted.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&output_file, "granted");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display(),
        );
        let request = json!({
            "id": "turn",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }

    let permission_request = child.expect_event("turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();
    assert_eq!(permission_request["threadId"], thread_id);
    assert_eq!(
        permission_request["permissions"]["fileSystem"]["write"][0],
        extra.display().to_string()
    );

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "permission-response",
            "method": "permission/respond",
            "params": {
                "requestId": request_id,
                "decision": "allow",
                "scope": "turn",
                "permissions": {"fileSystem": {"write": [extra], "read": null}, "network": null}
            }
        });
        writeln!(stdin, "{request}").expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let resolved = child.expect_event("permission-response", "permission_resolved");
    assert_eq!(resolved["requestId"], request_id);
    assert_eq!(resolved["decision"], "allow");
    let _turn_completed = child.expect_event("turn", "turn_completed");
    assert_eq!(std::fs::read_to_string(&output_file).unwrap(), "granted");

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_input_eof_cancels_pending_permission_request() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        let command = platform_shell_script("true", "$null");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display()
        );
        let request = json!({
            "id": "turn",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    child.expect_event("turn", "permission_request");

    child.close_stdin();
    let output = wait_for_child_output_with_timeout(child, Duration::from_secs(2))
        .expect("server exited after permission waiter was cancelled");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn server_mode_input_eof_cancels_pending_user_input_request() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = child.expect_event("thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"ask Continue?"}}]}}}}"#,
            thread_id,
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    child.expect_event("turn", "user_input_request");

    child.close_stdin();
    let output = wait_for_child_output_with_timeout(child, Duration::from_secs(2))
        .expect("server exited after user input waiter was cancelled");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn server_mode_request_permissions_propagates_strict_auto_review() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let output_file = extra.join("granted.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&output_file, "granted");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display(),
        );
        let request = json!({
            "id": "turn",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let permission_request = child.expect_event("turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "permission-response",
            "method": "permission/respond",
            "params": {
                "requestId": request_id,
                "decision": "allow",
                "scope": "turn",
                "strictAutoReview": true,
                "permissions": {"fileSystem": {"write": [extra], "read": null}, "network": null}
            }
        });
        writeln!(stdin, "{request}").expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let resolved = child.expect_event("permission-response", "permission_resolved");
    assert_eq!(resolved["strictAutoReview"], true);
    let completed_request_permissions = child.expect_event("turn", "tool_completed");
    assert_eq!(completed_request_permissions["tool"], "request_permissions");
    let output: Value = serde_json::from_str(
        completed_request_permissions["output"]
            .as_str()
            .expect("permission output"),
    )
    .expect("permission output json");
    assert_eq!(output["strictAutoReview"], true);
    let tool_approval = child.expect_event("turn", "permission_request");
    assert_eq!(tool_approval["reason"], "bash requested shell");
    assert_eq!(tool_approval["permissions"], json!({}));
    let tool_approval_id = tool_approval["requestId"]
        .as_str()
        .expect("tool approval request id");
    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "tool-approval-response",
            "method": "permission/respond",
            "params": {
                "requestId": tool_approval_id,
                "decision": "deny"
            }
        });
        writeln!(stdin, "{request}").expect("write tool approval denial");
        stdin.flush().expect("flush tool approval denial");
    }
    let tool_approval_resolved =
        child.expect_event("tool-approval-response", "permission_resolved");
    assert_eq!(tool_approval_resolved["requestId"], tool_approval_id);
    assert_eq!(tool_approval_resolved["decision"], "deny");
    let events = child.drain_events_until_event("turn", "turn_completed");
    assert_eq!(
        events
            .last()
            .and_then(|event| event["status"].as_str())
            .expect("turn status"),
        "approval_required"
    );
    assert!(!output_file.exists());

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_strict_auto_review_prompts_subsequent_command() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let output_file = extra.join("blocked.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&output_file, "blocked");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display(),
        );
        let request = json!({
            "id": "turn",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let permission_request = child.expect_event("turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "permission-response",
            "method": "permission/respond",
            "params": {
                "requestId": request_id,
                "decision": "allow",
                "scope": "turn",
                "strictAutoReview": true,
                "permissions": {"fileSystem": {"write": [extra], "read": null}, "network": null}
            }
        });
        writeln!(stdin, "{request}").expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let _resolved = child.expect_event("permission-response", "permission_resolved");
    let tool_approval = child.expect_event("turn", "permission_request");
    assert_eq!(tool_approval["reason"], "bash requested shell");
    assert_eq!(tool_approval["permissions"], json!({}));
    let tool_approval_id = tool_approval["requestId"]
        .as_str()
        .expect("tool approval request id");
    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "tool-approval-response",
            "method": "permission/respond",
            "params": {
                "requestId": tool_approval_id,
                "decision": "deny"
            }
        });
        writeln!(stdin, "{request}").expect("write tool approval denial");
        stdin.flush().expect("flush tool approval denial");
    }
    let tool_approval_resolved =
        child.expect_event("tool-approval-response", "permission_resolved");
    assert_eq!(tool_approval_resolved["requestId"], tool_approval_id);
    assert_eq!(tool_approval_resolved["decision"], "deny");
    let completed = child.expect_event("turn", "turn_completed");
    assert_eq!(completed["status"], "approval_required");
    assert!(
        !output_file.exists(),
        "strictAutoReview should stop the subsequent bash before execution"
    );

    child.close_stdin();
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_and_searches_threads() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"cli thread search needle"}}]}}}}"#,
            thread_id
        )
        .expect("write thread turn");
        stdin.flush().expect("flush thread turn");
    }
    child.expect_event("turn", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
        stdin.flush().expect("flush thread/list");
    }
    let listed = child.expect_event("list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    assert!(
        listed_threads.iter().any(|thread| {
            thread["threadId"] == thread_id
                && thread["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty())
        }),
        "thread/list did not include {thread_id}: {listed}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"search","method":"thread/search","params":{{"searchTerm":"needle","limit":10}}}}"#
        )
        .expect("write thread/search");
    }

    let searched = child.expect_event("search", "thread_search");
    let hits = searched["data"].as_array().expect("thread search data");
    assert!(hits.iter().any(|hit| {
        hit["thread"]["threadId"] == thread_id
            && hit["snippet"]
                .as_str()
                .is_some_and(|snippet| snippet.contains("needle"))
    }));

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/turns/list");
        stdin.flush().expect("flush thread/turns/list");
    }
    let turns = child.expect_event("turns", "thread_turns_list");
    let turn_data = turns["data"].as_array().expect("thread turns data");
    assert!(turn_data.iter().any(|turn| {
        turn["threadId"] == thread_id
            && turn["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user"
                        && item["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("needle"))
                }) && items.iter().any(|item| item["type"] == "agent_message")
            })
    }));

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read-turns","method":"thread/read","params":{{"threadId":"{}","includeTurns":true}}}}"#,
            thread_id
        )
        .expect("write thread/read includeTurns");
        stdin.flush().expect("flush thread/read includeTurns");
    }
    let read_turns = child.expect_event("read-turns", "thread_read");
    let read_turn_data = read_turns["turns"].as_array().expect("read turns data");
    assert!(read_turn_data.iter().any(|turn| {
        turn["threadId"] == thread_id
            && turn["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user"
                        && item["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("needle"))
                }) && items.iter().any(|item| item["type"] == "agent_message")
            })
    }));

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/items/list");
    }
    child.close_stdin();
    let items = child.expect_event("items", "thread_items_list");
    let item_data = items["data"].as_array().expect("thread items data");
    assert!(item_data.iter().any(|item| {
        item["threadId"] == thread_id
            && item["item"]["role"] == "user"
            && item["item"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("needle"))
    }));

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_persists_directory_grant() {
    let parent = sandbox_test_parent("orca-request-permissions-session-");
    let workspace = parent.path().join("workspace");
    let home = parent.path().join("home");
    let extra = parent.path().join("extra");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let first_output = extra.join("first.txt");
    let second_output = extra.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&first_output, "first");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display(),
        );
        let request = json!({
            "id": "turn-1",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = child.expect_event("turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "permission-response",
            "method": "permission/respond",
            "params": {
                "requestId": request_id,
                "decision": "allow",
                "scope": "session",
                "permissions": {"fileSystem": {"write": [extra], "read": null}, "network": null}
            }
        });
        writeln!(stdin, "{request}").expect("write session permission/respond");
        stdin.flush().expect("flush session permission/respond");
    }
    let _resolved = child.expect_event("permission-response", "permission_resolved");
    let _first_completed = child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        let unix_command = format!("printf ok > {}", unix_shell_path(&second_output));
        let windows_command = platform_write_file_command(&second_output, "ok");
        let request = json!({
            "id": "cmd-2",
            "method": "command/exec",
            "params": {
                "threadId": thread_id,
                "command": platform_shell_script(&unix_command, &windows_command)
            }
        });
        writeln!(stdin, "{request}").expect("write second command/exec");
        stdin.flush().expect("flush second command/exec");
    }
    let _second_completed = child.expect_event("cmd-2", "command_exec_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    child.close_stdin();
    let read = child.expect_event("read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        extra.display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(&first_output).expect("first output"),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(&second_output).expect("second output"),
        "ok"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_accepts_file_system_entries() {
    let parent = sandbox_test_parent("orca-request-permissions-entries-");
    let workspace = parent.path().join("workspace");
    let home = parent.path().join("home");
    let extra = parent.path().join("extra");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let first_output = extra.join("first.txt");
    let second_output = extra.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&first_output, "first");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            extra.display(),
        );
        let request = json!({
            "id": "turn-1",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = child.expect_event("turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "permission-response",
            "method": "permission/respond",
            "params": {
                "requestId": request_id,
                "decision": "allow",
                "scope": "session",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": null,
                        "entries": [{"path": extra, "access": "write"}]
                    },
                    "network": null
                }
            }
        });
        writeln!(stdin, "{request}").expect("write session permission/respond with entries");
        stdin
            .flush()
            .expect("flush session permission/respond with entries");
    }
    let resolved = child.expect_event("permission-response", "permission_resolved");
    assert_eq!(resolved["scope"], "session");
    let _first_completed = child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        let unix_command = format!("printf ok > {}", unix_shell_path(&second_output));
        let windows_command = platform_write_file_command(&second_output, "ok");
        let request = json!({
            "id": "cmd-2",
            "method": "command/exec",
            "params": {
                "threadId": thread_id,
                "command": platform_shell_script(&unix_command, &windows_command)
            }
        });
        writeln!(stdin, "{request}").expect("write second command/exec");
        stdin.flush().expect("flush second command/exec");
    }
    let _second_completed = child.expect_event("cmd-2", "command_exec_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    child.close_stdin();
    let read = child.expect_event("read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        extra.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_accepts_workspace_roots_entries() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let docs = workspace.path().join("docs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&docs).expect("create docs");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let first_output = docs.join("first.txt");
    let second_output = docs.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&first_output, "first");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            docs.display(),
        );
        let request = json!({
            "id": "turn-1",
            "method": "turn/start",
            "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}
        });
        writeln!(stdin, "{request}").expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = child.expect_event("turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"read":null,"write":null,"entries":[{{"path":{{"type":"special","value":{{"kind":"project_roots","subpath":"docs"}}}},"access":"write"}}]}},"network":null}}}}}}"#,
            request_id,
        )
        .expect("write session permission/respond with special entry");
        stdin
            .flush()
            .expect("flush session permission/respond with special entry");
    }
    let resolved = child.expect_event("permission-response", "permission_resolved");
    assert_eq!(resolved["scope"], "session");
    let _first_completed = child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        let unix_command = format!("printf ok > {}", unix_shell_path(&second_output));
        let windows_command = platform_write_file_command(&second_output, "ok");
        let request = json!({
            "id": "cmd-2",
            "method": "command/exec",
            "params": {
                "threadId": thread_id,
                "command": platform_shell_script(&unix_command, &windows_command)
            }
        });
        writeln!(stdin, "{request}").expect("write second command/exec");
        stdin.flush().expect("flush second command/exec");
    }
    let _second_completed = child.expect_event("cmd-2", "command_exec_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    child.close_stdin();
    let read = child.expect_event("read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        docs.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_rebinds_runtime_workspace_roots_for_permission_grants() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let old_root = workspace.path().join("old-root");
    let new_root = workspace.path().join("new-root");
    let docs = new_root.join("docs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(old_root.join("docs")).expect("create old docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    std::fs::write(
        home.join("config.toml"),
        "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"**\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let first_output = docs.join("first.txt");
    let second_output = docs.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        let request = json!({
            "id": "thread-req",
            "method": "thread/start",
            "params": {"runtimeWorkspaceRoots": [old_root]}
        });
        writeln!(stdin, "{request}").expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        let command = platform_write_file_command(&first_output, "first");
        let prompt = format!(
            "request_permissions_then_bash {} :: {command}",
            docs.display(),
        );
        let request = json!({
            "id": "turn-1",
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "runtimeWorkspaceRoots": [new_root],
                "input": [{"type": "text", "text": prompt}]
            }
        });
        writeln!(stdin, "{request}").expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = child.expect_event("turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"read":null,"write":null,"entries":[{{"path":{{"type":"special","value":{{"kind":"project_roots","subpath":"docs"}}}},"access":"write"}}]}},"network":null}}}}}}"#,
            request_id,
        )
        .expect("write session permission/respond with special entry");
        stdin
            .flush()
            .expect("flush session permission/respond with special entry");
    }
    let _resolved = child.expect_event("permission-response", "permission_resolved");
    let _first_completed = child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        let unix_command = format!("printf ok > {}", unix_shell_path(&second_output));
        let windows_command = platform_write_file_command(&second_output, "ok");
        let request = json!({
            "id": "cmd-2",
            "method": "command/exec",
            "params": {
                "threadId": thread_id,
                "command": platform_shell_script(&unix_command, &windows_command)
            }
        });
        writeln!(stdin, "{request}").expect("write second command/exec");
        stdin.flush().expect("flush second command/exec");
    }
    let _second_completed = child.expect_event("cmd-2", "command_exec_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    child.close_stdin();
    let read = child.expect_event("read", "thread_read");
    assert_eq!(
        read["runtimeWorkspaceRoots"][0],
        new_root.display().to_string()
    );
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        docs.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resumes_and_forks_persisted_threads() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = child.expect_event("thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#,
            parent_id
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    child.expect_event("turn-1", "turn_completed");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = child.expect_next_for_id("resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();
    assert_eq!(resumed_id, parent_id);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            resumed_id
        )
        .expect("write resumed turn");
        stdin.flush().expect("flush resumed turn");
    }
    let resumed_events = child.drain_events_until_event("turn-2", "turn_completed");
    let echoed = resumed_events
        .iter()
        .filter(|event| event["id"] == "turn-2" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        echoed.contains("first prompt | mock_history_echo"),
        "expected resumed thread to see persisted history, got: {echoed}"
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"read-resumed","method":"thread/read","params":{{"threadId":"{}","includeMessages":true}}}}"#,
            parent_id
        )
        .expect("write resumed thread/read");
        stdin.flush().expect("flush resumed thread/read");
    }
    let read_resumed = child.expect_event("read-resumed", "thread_read");
    assert!(
        read_resumed["messages"]
            .as_array()
            .expect("resumed messages")
            .iter()
            .any(|message| {
                message["role"] == "user" && message["content"] == "mock_history_echo"
            })
    );

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = child.expect_next_for_id("fork");
    assert_eq!(forked["event"], "thread_started");
    let child_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();
    assert_ne!(child_id, parent_id);

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"list-children","method":"thread/list","params":{{"parentThreadId":"{}","limit":10}}}}"#,
            parent_id
        )
        .expect("write child list");
    }
    child.close_stdin();
    let children = child.expect_event("list-children", "thread_list");
    let child_threads = children["data"].as_array().expect("child threads");
    assert!(child_threads.iter().any(|thread| {
        thread["threadId"] == child_id
            && thread["parentId"] == parent_id
            && thread["forked"] == true
    }));

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_filters_thread_list_by_codex_metadata_fields() {
    with_orca_home(|home| {
        let alpha_cwd = home.join("alpha");
        let beta_cwd = home.join("beta");
        std::fs::create_dir_all(&alpha_cwd).expect("alpha cwd");
        std::fs::create_dir_all(&beta_cwd).expect("beta cwd");

        let store = SessionStore::new();
        let mut parent = store
            .start_writer_from_meta(store.create_meta(
                &alpha_cwd,
                "deepseek",
                Some("deepseek-v4-flash".to_string()),
                "server filter parent",
            ))
            .expect("parent writer");
        parent.complete("success").expect("complete parent");
        let parent_id = store
            .list_sessions_with_archived(1, false)
            .expect("list parent")[0]
            .session_id
            .clone();

        let child_meta = store.create_fork_meta(
            &beta_cwd,
            "openai",
            Some("gpt-5".to_string()),
            "server filter child",
            parent_id.clone(),
        );
        let child_id = child_meta.session_id.clone();
        let mut child_writer = store
            .start_writer_from_meta(child_meta)
            .expect("child writer");
        child_writer.complete("success").expect("complete child");

        let archived_meta = store.create_meta(
            &beta_cwd,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
            "server filter archived",
        );
        let archived_id = archived_meta.session_id.clone();
        let mut archived_writer = store
            .start_writer_from_meta(archived_meta)
            .expect("archived writer");
        archived_writer
            .complete("success")
            .expect("complete archived");
        store
            .archive_session(&archived_id)
            .expect("archive prepared thread");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin_mut();
            let request = json!({
                "id": "filter-cwd",
                "method": "thread/list",
                "params": {
                    "cwd": alpha_cwd,
                    "limit": 10,
                    "sortKey": "createdAt",
                    "sortDirection": "asc"
                }
            });
            writeln!(stdin, "{request}").expect("write cwd filter");
            stdin.flush().expect("flush cwd filter");
        }
        let filtered_cwd = child.expect_event("filter-cwd", "thread_list");
        let cwd_threads = filtered_cwd["data"].as_array().expect("cwd data");
        assert_eq!(cwd_threads.len(), 1);
        assert_eq!(cwd_threads[0]["threadId"], parent_id);

        {
            let stdin = child.stdin_mut();
            writeln!(
            stdin,
            r#"{{"id":"filter-provider-model","method":"thread/list","params":{{"modelProviders":["openai"],"model":["gpt-5"],"limit":10}}}}"#
        )
        .expect("write provider model filter");
            stdin.flush().expect("flush provider model filter");
        }
        let filtered_provider = child.expect_event("filter-provider-model", "thread_list");
        let provider_threads = filtered_provider["data"].as_array().expect("provider data");
        assert_eq!(provider_threads.len(), 1);
        assert_eq!(provider_threads[0]["threadId"], child_id);

        {
            let stdin = child.stdin_mut();
            writeln!(
            stdin,
            r#"{{"id":"filter-child","method":"thread/list","params":{{"parentThreadId":"{}","limit":10}}}}"#,
            parent_id
        )
        .expect("write relation filter");
            stdin.flush().expect("flush relation filter");
        }
        let filtered_child = child.expect_event("filter-child", "thread_list");
        let child_threads = filtered_child["data"].as_array().expect("child data");
        assert_eq!(child_threads.len(), 1);
        assert_eq!(child_threads[0]["threadId"], child_id);

        {
            let stdin = child.stdin_mut();
            writeln!(
            stdin,
            r#"{{"id":"filter-archived","method":"thread/list","params":{{"archived":true,"limit":10}}}}"#
        )
        .expect("write archived filter");
        }
        child.close_stdin();

        let filtered_archived = child.expect_event("filter-archived", "thread_list");
        let archived_threads = filtered_archived["data"].as_array().expect("archived data");
        assert_eq!(archived_threads.len(), 1);
        assert_eq!(archived_threads[0]["threadId"], archived_id);
        assert_eq!(archived_threads[0]["archived"], true);

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_rejects_turn_start_for_unknown_thread() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        writeln!(
            stdin,
            r#"{{"id":"turn-req","method":"turn/start","params":{{"threadId":"missing-thread","input":[{{"type":"text","text":"hello missing thread"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], "turn-req");
    assert_eq!(events[0]["event"], "error");
    assert_eq!(events[0]["message"], "unknown thread: missing-thread");
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

fn wait_for_child_output_with_timeout(
    child: ServerTestClient,
    timeout: Duration,
) -> Result<Output, String> {
    child
        .wait_with_output_timeout(timeout)
        .map_err(|error| error.to_string())
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for path: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_command_exec_error(params: Value, expected_message: &str) {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin_mut();
        let request = json!({"id": "cmd", "method": "command/exec", "params": params});
        writeln!(stdin, "{request}").expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    child.close_stdin();

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "unexpected server stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1, "expected one error event, got {events:?}");
    assert_eq!(events[0]["id"], "cmd");
    assert_eq!(
        events[0]["event"], "error",
        "expected command/exec error for params {params}, got {events:?}"
    );
    assert_eq!(
        events[0]["message"], expected_message,
        "unexpected command/exec error for params {params}"
    );
}

#[cfg(unix)]
fn read_shell_read_result(client: &mut ServerTestClient, id: &str) -> Value {
    loop {
        let event = client.expect_next_for_id(id);
        match event["event"].as_str() {
            Some("shell_output_delta") => {
                assert!(event["shellId"].as_str().is_some());
                assert!(
                    matches!(event["stream"].as_str(), Some("stdout") | Some("stderr")),
                    "unexpected shell output stream: {event}"
                );
                assert!(event["delta"].as_str().is_some());
                assert!(event["final"].is_boolean());
            }
            Some("shell_exited") => {
                assert!(event["shellId"].as_str().is_some());
                assert!(event["taskId"].as_str().is_some());
                assert!(event["status"].as_str().is_some());
                assert!(event["exitCode"].is_number() || event["exitCode"].is_null());
            }
            Some("shell_updated") | Some("shell_completed") => return event,
            other => panic!("unexpected shell/read event {other:?}: {event}"),
        }
    }
}

fn read_events_until_shell_read_response(client: &mut ServerTestClient, id: &str) -> Vec<Value> {
    client.drain_events_until_matching(&format!("shell/read response for {id}"), |event| {
        event["id"] == id
            && matches!(
                event["event"].as_str(),
                Some("shell_updated" | "shell_completed" | "error")
            )
    })
}

fn read_command_exec_output_until(
    client: &mut ServerTestClient,
    process_id: &str,
    predicate: impl Fn(&str, &str) -> bool,
) -> Vec<Value> {
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    client.drain_events_until_matching("command/exec streaming output", |event| {
        if event["event"] == "command_exec_output_delta" && event["processId"] == process_id {
            let stream = event["stream"].as_str().unwrap_or_default();
            let delta = event["delta"].as_str().unwrap_or_default();
            match stream {
                "stdout" => stdout_text.push_str(delta),
                "stderr" => stderr_text.push_str(delta),
                other => panic!("unexpected command/exec stream {other}: {event}"),
            }
            predicate(&stdout_text, &stderr_text)
        } else {
            false
        }
    })
}

fn assert_command_exec_delta_seen(events: &[Value], stream: &str, expected_delta: &str) {
    assert!(
        command_exec_events_contain(events, stream, expected_delta),
        "missing command/exec {stream} delta containing {expected_delta:?}: {events:?}"
    );
}

fn command_exec_events_contain(events: &[Value], stream: &str, expected_delta: &str) -> bool {
    events.iter().any(|event| {
        event["event"] == "command_exec_output_delta"
            && event["stream"] == stream
            && event["delta"]
                .as_str()
                .is_some_and(|delta| delta.contains(expected_delta))
    })
}

fn assert_command_exec_output_delta_notification_seen(
    events: &[Value],
    stream: &str,
    process_id: &str,
) {
    assert!(
        events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["method"] == "command/exec/outputDelta"
                && event["params"]["processId"] == process_id
                && event["params"]["stream"] == stream
                && event["params"]["deltaBase64"].as_str().is_some()
                && event["params"]["capReached"].is_boolean()
        }),
        "missing command/exec outputDelta notification shape for {process_id}/{stream}: {events:?}"
    );
}

fn read_events_until_workflow_item_completed(
    client: &mut ServerTestClient,
    id: &str,
) -> Vec<Value> {
    client.drain_events_until_matching(&format!("workflow item completion for {id}"), |event| {
        event["id"] == id
            && event["event"] == "item_completed"
            && event["item"]["type"] == "workflow"
    })
}

fn has_event(events: &[Value], event: &str) -> bool {
    events.iter().any(|value| value["event"] == event)
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = lock_env();
    let home = tempdir().expect("temp home");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = f(home.path());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    result
}

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_sleep_hook_config(home: &std::path::Path, seconds: f32) {
    std::fs::create_dir_all(home).expect("create ORCA_HOME");
    let command = platform_shell_script(
        &format!("sleep {seconds}"),
        &format!(
            "Start-Sleep -Milliseconds {}",
            (seconds * 1000.0).round() as u64
        ),
    )
    .to_string();
    std::fs::write(
        home.join("config.toml"),
        format!("[[hooks]]\nevent = \"pre_model_call\"\ncommand = \"{command}\"\n"),
    )
    .expect("write hook config");
}

fn shell_escape(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_slow_mcp_server(dir: &std::path::Path) -> std::path::PathBuf {
    let server = dir.join("slow_mcp_server.sh");
    std::fs::write(
        &server,
        r#"#!/bin/sh
log_file="${1:-}"
while IFS= read -r line; do
  if [ -n "$log_file" ]; then
    printf '%s\n' "$line" >> "$log_file"
  fi
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      sleep 5
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      ;;
  esac
done
"#,
    )
    .expect("write MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }
    server
}

#[cfg(unix)]
fn write_resource_mcp_server(dir: &std::path::Path) -> std::path::PathBuf {
    let server = dir.join("resource_mcp_server.sh");
    std::fs::write(
        &server,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://orca/one","name":"memo one","description":"A test memo","mimeType":"text/plain"}]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":4,"result":{"resourceTemplates":[]}}\n'
      ;;
    *'"method":"resources/read"'*)
      printf '{"jsonrpc":"2.0","id":5,"result":{"contents":[{"uri":"memo://orca/one","mimeType":"text/plain","text":"resource body from shared registry"}]}}\n'
      ;;
  esac
done
"#,
    )
    .expect("write resource MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }
    server
}

#[cfg(unix)]
fn write_eliciting_mcp_server(dir: &std::path::Path) -> std::path::PathBuf {
    let server = dir.join("eliciting_mcp_server.sh");
    std::fs::write(
        &server,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicits","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits for elicitation","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize wait","url":"https://example.test/device","requestedSchema":{"type":"object","properties":{"code":{"type":"string"}}}}}\n'
      IFS= read -r response
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"after elicitation"}],"isError":false}}\n'
      ;;
  esac
done
"#,
    )
    .expect("write eliciting MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }
    server
}
