use orca_runtime::task_output::{TaskOutputRead, TaskOutputStore};
use orca_runtime::{
    shell_session::{
        RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
    },
    tasks::TaskRegistry,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn platform_shell_script(unix: &str, windows: &str) -> String {
    if cfg!(windows) {
        windows.to_string()
    } else {
        unix.to_string()
    }
}

#[test]
fn task_output_store_reads_delta_and_tail_without_splitting_utf8() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-1";

    store
        .append_stdout(task_id, "first\n")
        .expect("append stdout");
    store
        .append_stderr(task_id, "错误\n")
        .expect("append stderr");
    store
        .append_stdout(task_id, "last\n")
        .expect("append stdout again");

    assert_eq!(store.size(task_id), 18);

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "first\nlast\n");
    assert_eq!(snapshot.stderr, "错误\n");
    assert_eq!(snapshot.combined, "first\n错误\nlast\n");

    let delta = store.read_delta(task_id, 6, 7).expect("read delta");
    assert_eq!(
        delta,
        TaskOutputRead {
            stdout: String::new(),
            stderr: "错误\n".to_string(),
            combined: "错误\n".to_string(),
            next_offset: 13,
            bytes_read: 7,
            bytes_total: 18,
            omitted_prefix_bytes: 0,
            stdout_prefix_bytes: 6,
            stderr_prefix_bytes: 0,
        }
    );

    let tail = store.tail(task_id, 6).expect("read tail");
    assert_eq!(tail.stdout, "last\n");
    assert_eq!(tail.stderr, "");
    assert_eq!(tail.combined, "last\n");
    assert_eq!(tail.next_offset, 18);
    assert_eq!(tail.bytes_read, 5);
    assert_eq!(tail.bytes_total, 18);
    assert_eq!(tail.omitted_prefix_bytes, 13);
    assert_eq!(tail.stdout_prefix_bytes, 6);
    assert_eq!(tail.stderr_prefix_bytes, 7);
}

#[test]
fn task_output_store_skips_partial_utf8_at_delta_start() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-utf8-start";

    store
        .append_stdout(task_id, "a错误b")
        .expect("append stdout");

    let delta = store
        .read_delta(task_id, 2, 64)
        .expect("read from inside utf8 codepoint");

    assert_eq!(delta.stdout, "误b");
    assert_eq!(delta.stderr, "");
    assert_eq!(delta.combined, "误b");
    assert_eq!(delta.next_offset, store.size(task_id));
    assert_eq!(delta.bytes_read, store.size(task_id) - 2);
}

#[test]
fn task_output_store_advances_when_delta_cap_splits_first_utf8_codepoint() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-utf8-cap";

    store.append_stdout(task_id, "错误").expect("append stdout");

    let delta = store
        .read_delta(task_id, 0, 2)
        .expect("read capped inside utf8 codepoint");

    assert_eq!(delta.stdout, "错");
    assert_eq!(delta.stderr, "");
    assert_eq!(delta.combined, "错");
    assert_eq!(delta.next_offset, 3);
    assert_eq!(delta.bytes_read, 3);
    assert_eq!(delta.bytes_total, store.size(task_id));
}

#[test]
fn task_output_store_retains_bounded_tail_and_reports_omitted_prefix() {
    let store = TaskOutputStore::with_max_retained_bytes(5);
    let task_id = "task-output-bounded-tail";

    store
        .append_stdout(task_id, "first\n")
        .expect("append stdout");
    store
        .append_stderr(task_id, "错误\n")
        .expect("append stderr");
    store
        .append_stdout(task_id, "last\n")
        .expect("append stdout again");

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "last\n");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.combined, "last\n");
    assert_eq!(snapshot.next_offset, 18);
    assert_eq!(snapshot.bytes_read, 5);
    assert_eq!(snapshot.bytes_total, 18);
    assert_eq!(snapshot.omitted_prefix_bytes, 13);
    assert_eq!(snapshot.stdout_prefix_bytes, 6);
    assert_eq!(snapshot.stderr_prefix_bytes, 7);

    let tail = store.tail(task_id, 64).expect("read tail");
    assert_eq!(tail.stdout, "last\n");
    assert_eq!(tail.stderr, "");
    assert_eq!(tail.combined, "last\n");
    assert_eq!(tail.omitted_prefix_bytes, 13);
    assert_eq!(tail.stdout_prefix_bytes, 6);
    assert_eq!(tail.stderr_prefix_bytes, 7);
}

#[test]
fn task_output_store_trims_to_utf8_boundary_when_cap_splits_multibyte_character() {
    let store = TaskOutputStore::with_max_retained_bytes(4);
    let task_id = "task-output-bounded-utf8";

    store
        .append_stdout(task_id, "a错误b")
        .expect("append stdout");

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "误b");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.combined, "误b");
    assert_eq!(snapshot.next_offset, store.size(task_id));
    assert_eq!(snapshot.bytes_total, 8);
    assert_eq!(snapshot.omitted_prefix_bytes, 4);
    assert_eq!(snapshot.stdout_prefix_bytes, 4);
    assert_eq!(snapshot.stderr_prefix_bytes, 0);
}

#[test]
fn task_output_store_remove_drops_task_buffer() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-remove";

    store
        .append_stdout(task_id, "output")
        .expect("append stdout");
    assert_eq!(store.size(task_id), 6);

    assert!(store.remove(task_id));
    assert_eq!(store.size(task_id), 0);

    let snapshot = store.read_delta(task_id, 0, 64).expect("read removed");
    assert_eq!(snapshot.stdout, "");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.next_offset, 0);
}

#[test]
fn shell_session_writes_process_output_to_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-store".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script(
                "printf stdout; printf stderr >&2; read -r _ || true",
                "[Console]::Out.Write('stdout'); [Console]::Out.Flush(); [Console]::Error.Write('stderr'); [Console]::Error.Flush(); $null = [Console]::In.ReadLine()",
            ),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "capture output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let deadline = Instant::now() + Duration::from_secs(5);
    let running_output = loop {
        let output = manager
            .read(&handle.id, Duration::from_millis(50))
            .expect("read running shell");
        if output.stdout == "stdout" && output.stderr == "stderr" {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "both shell readers did not publish output"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(running_output.stdout, "stdout");
    assert_eq!(running_output.stderr, "stderr");

    let stored = manager
        .output_store()
        .read_delta(&handle.task_id, 0, 64)
        .expect("stored output");
    assert_eq!(stored.stdout, "stdout");
    assert_eq!(stored.stderr, "stderr");

    manager
        .write_stdin(&handle.id, "\n")
        .expect("release running shell");
    let final_output = manager
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait for shell");
    assert_eq!(final_output.stdout, "stdout");
    assert_eq!(final_output.stderr, "stderr");
}

#[test]
fn shell_session_evicts_completed_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script("printf done", "[Console]::Out.Write('done')"),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "evict output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let output = manager
        .wait(&handle.id, Duration::from_secs(2))
        .expect("wait for shell");

    assert_eq!(output.stdout, "done");
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_evicts_completed_process_output_when_read_observes_exit() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-read-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script("printf done", "[Console]::Out.Write('done')"),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "read evicts output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let deadline = Instant::now() + Duration::from_secs(5);
    let output = loop {
        let output = manager
            .read(&handle.id, Duration::from_millis(100))
            .expect("read completed shell");
        if output.status != orca_core::task_types::TaskStatus::Running {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "shell did not complete before read deadline"
        );
    };

    assert_eq!(output.stdout, "done");
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_reap_completed_removes_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-list-reap".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script("printf listed", "[Console]::Out.Write('listed')"),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "list reaps output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let deadline = Instant::now() + Duration::from_secs(5);
    let completed = loop {
        let completed = manager.reap_completed().expect("reap completed shell");
        if !completed.is_empty() {
            break completed;
        }
        assert!(
            Instant::now() < deadline,
            "shell did not complete before reap deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].id, handle.id);
    assert!(
        manager.list().iter().all(|shell| shell.id != handle.id),
        "completed shell should be removed after explicit reap"
    );
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_evicts_stopped_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-kill-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script(
                "printf running; sleep 5",
                "[Console]::Out.Write('running'); [Console]::Out.Flush(); Start-Sleep -Seconds 5",
            ),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "kill evicts output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stored = manager
            .output_store()
            .read_delta(&handle.task_id, 0, 64)
            .expect("read running output");
        if stored.stdout.contains("running") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shell did not publish output before kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = manager.kill(&handle.id).expect("kill shell");

    assert_eq!(output.status, orca_core::task_types::TaskStatus::Stopped);
    assert!(output.stdout.contains("running"));
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_reports_when_retained_output_omits_prefix() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-cap".to_string());
    let output_store = TaskOutputStore::with_max_retained_bytes(5);
    let mut manager = RuntimeShellSessionManager::with_output_store(registry, output_store);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script(
                "printf 'first\\nlast\\n'",
                "[Console]::Out.Write(\"first`nlast`n\")",
            ),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "cap output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let output = manager
        .wait(&handle.id, Duration::from_secs(5))
        .expect("wait for shell");

    assert_eq!(output.stdout, "[6 bytes of earlier output omitted]\nlast\n");
}

#[test]
fn persistent_shell_archives_large_live_output_and_reopens_after_completion() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let root = cwd.path().join("tasks");
    let registry = TaskRegistry::new_persistent("large-process".to_string(), root.clone()).unwrap();
    let mut manager = RuntimeShellSessionManager::new(registry);
    let started = Instant::now();
    let handle = manager
        .spawn(ShellSessionCommand {
            command: platform_shell_script(
                "head -c 9437184 /dev/zero | tr '\\000' x; printf err >&2",
                "[Console]::Out.Write(('x' * 9437184)); [Console]::Error.Write('err')",
            ),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "large durable output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .unwrap();
    let output = manager.wait(&handle.id, Duration::from_secs(30)).unwrap();
    assert_eq!(output.status, orca_core::task_types::TaskStatus::Completed);
    assert!(output.stdout.len() <= 8 * 1024 * 1024 + 100);
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
    drop(manager);

    let registry = TaskRegistry::new_persistent("large-process".to_string(), root).unwrap();
    let manager = RuntimeShellSessionManager::new(registry);
    let store = manager.output_store();
    let mut offset = 0;
    let mut stdout_bytes = 0;
    let mut stderr = String::new();
    loop {
        let page = store
            .read_delta(&handle.task_id, offset, 64 * 1024)
            .unwrap();
        assert!(page.stdout.bytes().all(|byte| byte == b'x'));
        assert_eq!(page.omitted_prefix_bytes, 0);
        stdout_bytes += page.stdout.len();
        stderr.push_str(&page.stderr);
        assert!(page.next_offset > offset || page.next_offset == page.bytes_total);
        offset = page.next_offset;
        if offset == page.bytes_total {
            break;
        }
    }
    assert_eq!(stdout_bytes, 9 * 1024 * 1024);
    assert_eq!(stderr, "err");
    assert_eq!(offset, stdout_bytes + 3);
    let elapsed = started.elapsed();
    println!(
        "durable 9 MiB process capture + reopen/read: {elapsed:?}, {:.2} MiB/s",
        9.0 / elapsed.as_secs_f64()
    );
}

#[cfg(unix)]
#[test]
fn persistent_shell_rejects_unsafe_archive_before_starting_process() {
    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = cwd.path().join("tasks");
    let registry = TaskRegistry::new_persistent("unsafe-output".to_string(), root.clone()).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("unsafe-output/output")).unwrap();
    let mut manager = RuntimeShellSessionManager::new(registry);
    let error = manager
        .spawn(ShellSessionCommand {
            command: "printf launched > marker".to_string(),
            argv: None,
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "must not launch".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert!(!cwd.path().join("marker").exists());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn shell_task_registry_override_owns_archive_even_with_same_session_id() {
    let cwd = tempfile::tempdir().unwrap();
    let original_root = cwd.path().join("original");
    let override_root = cwd.path().join("override");
    let original =
        TaskRegistry::new_persistent("same-id".to_string(), original_root.clone()).unwrap();
    let override_registry =
        TaskRegistry::new_persistent("same-id".to_string(), override_root.clone()).unwrap();
    let mut manager = RuntimeShellSessionManager::new(original);
    let handle = manager
        .spawn_with_task_registry(
            ShellSessionCommand {
                command: platform_shell_script("printf scoped", "[Console]::Out.Write('scoped')"),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: BTreeMap::new(),
                description: "scoped override".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            },
            override_registry,
        )
        .unwrap();
    assert_eq!(
        manager
            .wait(&handle.id, Duration::from_secs(5))
            .unwrap()
            .stdout,
        "scoped"
    );
    assert!(
        manager
            .output_store()
            .read_delta(&handle.task_id, 0, 10)
            .is_err()
    );
    drop(manager);
    let reopened = RuntimeShellSessionManager::new(
        TaskRegistry::new_persistent("same-id".to_string(), override_root).unwrap(),
    );
    assert_eq!(
        reopened
            .output_store()
            .read_delta(&handle.task_id, 0, 10)
            .unwrap()
            .combined,
        "scoped"
    );
}
