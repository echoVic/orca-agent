#[cfg(windows)]
use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use orca_core::task_types::{TaskStatus, TaskType};
use orca_runtime::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellSessionTermination,
    ShellTerminalMode,
};
use orca_runtime::tasks::TaskRegistry;

#[cfg(windows)]
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(windows)]
struct WindowsSandboxTestHome {
    previous_home: Option<OsString>,
    _home: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(not(windows))]
struct WindowsSandboxTestHome;

#[cfg(windows)]
impl WindowsSandboxTestHome {
    fn new(workspace: &Path) -> Self {
        let lock = ORCA_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::Builder::new()
            .prefix("orca-shell-session-contract-")
            .tempdir()
            .expect("create isolated ORCA_HOME");
        let previous_home = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        orca_windows_sandbox::CapabilityStore::new(home.path().join("windows-capabilities"))
            .provision_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
            .expect("provision Windows sandbox setup for shell session workspace");
        Self {
            previous_home,
            _home: home,
            _lock: lock,
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsSandboxTestHome {
    fn drop(&mut self) {
        match self.previous_home.take() {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }
}

#[cfg(windows)]
fn prepare_windows_sandbox(workspace: &Path) -> WindowsSandboxTestHome {
    WindowsSandboxTestHome::new(workspace)
}

#[cfg(not(windows))]
fn prepare_windows_sandbox(_workspace: &Path) -> WindowsSandboxTestHome {
    WindowsSandboxTestHome
}

fn platform_shell_script(unix: &str, windows: &str) -> String {
    if cfg!(windows) {
        windows.to_string()
    } else {
        unix.to_string()
    }
}

fn platform_interactive_command(unix: &str, windows_node: &str) -> (String, Option<Vec<String>>) {
    #[cfg(windows)]
    {
        (
            windows_node.to_string(),
            Some(vec![
                "node".to_string(),
                "-e".to_string(),
                windows_node.to_string(),
            ]),
        )
    }
    #[cfg(not(windows))]
    {
        let _ = windows_node;
        (unix.to_string(), None)
    }
}

fn powershell_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn marker_wait_script(started: &Path, release: &Path) -> String {
    let unix_started = started.display();
    let unix_release = release.display();
    let windows_started = powershell_literal(started);
    let windows_release = powershell_literal(release);
    platform_shell_script(
        &format!(
            "printf started; : > {unix_started:?}; while [ ! -e {unix_release:?} ]; do sleep 0.05; done; printf done"
        ),
        &format!(
            "Write-Host -NoNewline 'started'; Set-Content -NoNewline -LiteralPath {windows_started} -Value ''; while (!(Test-Path -LiteralPath {windows_release})) {{ Start-Sleep -Milliseconds 50 }}; Write-Host -NoNewline 'done'"
        ),
    )
}

#[test]
fn shell_session_runs_interactive_stdin_and_records_task_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());
    let (command, argv) = platform_interactive_command(
        "read line; printf 'reply:%s\\n' \"$line\"",
        "let input = ''; process.stdin.setEncoding('utf8'); process.stdin.on('data', chunk => input += chunk); process.stdin.on('end', () => process.stdout.write(`reply:${input.trimEnd()}`));",
    );

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: command.clone(),
            argv,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "interactive echo".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");
    assert_eq!(handle.requested_terminal, ShellTerminalMode::pipe());
    assert_eq!(handle.effective_terminal, ShellTerminalMode::pipe());
    sessions
        .write_stdin(&handle.id, "hello-runtime\n")
        .expect("write stdin");
    sessions.close_stdin(&handle.id).expect("close stdin");

    let output = sessions
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait shell session");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout.trim(), "reply:hello-runtime");
    assert_eq!(output.stderr, "");
    let task = tasks.get(&handle.task_id).expect("shell task");
    assert_eq!(task.task_type, TaskType::Shell);
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(
        task.result.as_deref().map(str::trim),
        Some("reply:hello-runtime")
    );
    assert_eq!(task.error, None);
    assert_eq!(tasks.list()[0].command.as_deref(), Some(command.as_str()));
}

#[test]
fn shell_session_applies_environment_overrides_and_unsets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks);

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: platform_shell_script(
                "printf '%s|%s' \"$ORCA_SHELL_ENV_ADDED\" \"${ORCA_SHELL_ENV_REMOVED-unset}\"",
                "if ($null -eq $env:ORCA_SHELL_ENV_REMOVED) { $removed = 'unset' } else { $removed = $env:ORCA_SHELL_ENV_REMOVED }; Write-Host -NoNewline \"$env:ORCA_SHELL_ENV_ADDED|$removed\"",
            ),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: std::collections::BTreeMap::from([
                (
                    "ORCA_SHELL_ENV_ADDED".to_string(),
                    Some("added".to_string()),
                ),
                ("ORCA_SHELL_ENV_REMOVED".to_string(), None),
            ]),
            description: "env shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    let output = sessions
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait shell session");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "added|unset");
    assert_eq!(output.stderr, "");
}

#[cfg(target_os = "macos")]
#[test]
fn sandboxed_shell_session_cannot_override_seatbelt_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks);

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: "printf %s \"$ORCA_SANDBOX\"".to_string(),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: std::collections::BTreeMap::from([(
                "ORCA_SANDBOX".to_string(),
                Some("spoofed".to_string()),
            )]),
            description: "sandbox marker".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    let output = sessions
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait shell session");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "seatbelt");
}

#[test]
fn shell_session_kill_stops_running_task_and_collects_partial_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let started_marker = temp.path().join("shell-kill-started");
    let release_marker = temp.path().join("shell-kill-release");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: marker_wait_script(&started_marker, &release_marker),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "long shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    wait_for_path(&started_marker);
    let output = sessions.kill(&handle.id).expect("kill shell session");

    assert!(output.stdout.contains("started"));
    assert_ne!(output.exit_code, Some(0));
    let task = tasks.get(&handle.task_id).expect("shell task");
    assert_eq!(task.status, TaskStatus::Stopped);
    assert_eq!(task.result.as_deref(), Some(output.stdout.as_str()));
}

#[test]
fn shell_session_kill_preserves_already_exited_terminal_with_buffered_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());
    let handle = sessions
        .spawn(ShellSessionCommand {
            command: platform_shell_script("printf completed", "Write-Host -NoNewline 'completed'"),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "already completed shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell session");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sessions.output_store().size(&handle.task_id) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "output was not buffered"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    loop {
        let status = sessions
            .list()
            .into_iter()
            .find(|snapshot| snapshot.id == handle.id)
            .expect("shell snapshot")
            .status;
        if status != TaskStatus::Running {
            assert_eq!(status, TaskStatus::Completed);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "shell did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }

    let output = sessions.kill(&handle.id).expect("collect exited shell");

    assert_eq!(output.termination, ShellSessionTermination::Exited);
    assert_eq!(output.status, TaskStatus::Completed);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "completed");
    assert_eq!(
        tasks.get(&handle.task_id).expect("shell task").status,
        TaskStatus::Completed
    );
}

#[test]
fn shell_session_reaps_task_stop_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let started_marker = temp.path().join("shell-started");
    let release_marker = temp.path().join("shell-release");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: marker_wait_script(&started_marker, &release_marker),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "stoppable shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    wait_for_path(&started_marker);
    tasks.request_stop(&handle.task_id).expect("request stop");
    let stopped = sessions
        .reap_requested_stops()
        .expect("reap requested shell stops");

    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].id, handle.id);
    assert!(stopped[0].stdout.contains("started"));
    assert_ne!(stopped[0].exit_code, Some(0));
    assert_eq!(
        sessions
            .read(&handle.id, Duration::from_millis(1))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    let task = tasks.get(&handle.task_id).expect("shell task");
    assert_eq!(task.status, TaskStatus::Stopped);
    assert_eq!(task.result.as_deref(), Some(stopped[0].stdout.as_str()));
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for path: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shell_session_read_returns_incremental_output_without_waiting_for_exit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let started_marker = temp.path().join("shell-started");
    let release_marker = temp.path().join("shell-release");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: marker_wait_script(&started_marker, &release_marker),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "incremental shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    wait_for_path(&started_marker);
    let output_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sessions.output_store().size(&handle.task_id) == 0 {
        assert!(
            std::time::Instant::now() < output_deadline,
            "incremental shell output was not buffered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!release_marker.exists(), "shell was released before read");

    let output = sessions
        .read(&handle.id, Duration::ZERO)
        .expect("read shell session");
    let task_status = tasks.get(&handle.task_id).expect("shell task").status;
    sessions.kill(&handle.id).expect("cleanup shell session");

    assert!(
        !release_marker.exists(),
        "read returned only after the shell was released"
    );
    assert_eq!(output.exit_code, None);
    assert_eq!(output.status, TaskStatus::Running);
    assert_eq!(output.stdout, "started");
    assert_eq!(task_status, TaskStatus::Running);
}

#[test]
fn shell_session_list_returns_running_shell_snapshots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());
    let command = platform_shell_script(
        "printf ready; sleep 30",
        "Write-Host -NoNewline 'ready'; Start-Sleep -Seconds 30",
    );

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: command.clone(),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "listed shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    let snapshots = sessions.list();

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, handle.id);
    assert_eq!(snapshots[0].task_id, handle.task_id);
    assert_eq!(snapshots[0].command, command);
    assert_eq!(snapshots[0].description, "listed shell");
    assert_eq!(snapshots[0].status, TaskStatus::Running);
    assert_eq!(snapshots[0].requested_terminal, ShellTerminalMode::pipe());
    assert_eq!(snapshots[0].effective_terminal, ShellTerminalMode::pipe());

    sessions.kill(&handle.id).expect("cleanup shell session");
}

#[test]
fn shell_session_updates_description_for_list_snapshots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks);

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: platform_shell_script("sleep 30", "Start-Sleep -Seconds 30"),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "original shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");

    sessions
        .update_description(&handle.id, "renamed shell")
        .expect("update shell description");
    let snapshots = sessions.list();

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, handle.id);
    assert_eq!(snapshots[0].description, "renamed shell");

    sessions.kill(&handle.id).expect("cleanup shell session");
}

#[test]
fn shell_session_terminate_all_preserves_natural_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _windows_sandbox = prepare_windows_sandbox(temp.path());
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks.clone());
    let handle = sessions
        .spawn(ShellSessionCommand {
            command: platform_shell_script("printf completed", "Write-Host -NoNewline 'completed'"),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "naturally completed shell".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn shell session");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while sessions
        .list()
        .iter()
        .any(|snapshot| snapshot.id == handle.id && snapshot.status == TaskStatus::Running)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "shell did not complete"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    sessions.terminate_all();

    let task = tasks.get(&handle.task_id).expect("shell task");
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.result.as_deref(), Some("completed"));
}

#[cfg(unix)]
#[test]
fn shell_session_pty_exposes_terminal_to_child_process() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks);

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: "if test -t 0 && test -t 1; then printf tty; else printf pipe; fi".to_string(),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "pty shell".to_string(),
            terminal: ShellTerminalMode::pty(None, None),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn pty shell session");
    assert_eq!(
        handle.requested_terminal,
        ShellTerminalMode::pty(None, None)
    );
    assert_eq!(
        handle.effective_terminal,
        ShellTerminalMode::pty(None, None)
    );

    let output = sessions
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait pty shell session");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout.trim(), "tty");
}

#[cfg(unix)]
#[test]
fn shell_session_pty_starts_with_configured_window_size() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tasks = TaskRegistry::new("session-shell".to_string());
    let mut sessions = RuntimeShellSessionManager::new(tasks);

    let handle = sessions
        .spawn(ShellSessionCommand {
            command: "python3 -c 'import fcntl,termios,struct,sys; data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack(\"HHHH\",0,0,0,0)); rows,cols,_,_=struct.unpack(\"HHHH\", data); print(f\"{rows} {cols}\")'".to_string(),
            argv: None,
            cwd: temp.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            description: "sized pty shell".to_string(),
            terminal: ShellTerminalMode::pty(Some(132), Some(41)),
            sandbox: ShellSandboxMode::default(),
        })
        .expect("spawn sized pty shell session");

    let output = sessions
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait sized pty shell session");

    assert_eq!(output.exit_code, Some(0));
    assert!(
        output.stdout.contains("41 132"),
        "expected initial pty size 41 rows and 132 cols, got: {}",
        output.stdout
    );
}
