use std::fs;
use std::path::Path;

use orca_core::task_types::TaskStatus;
use orca_runtime::tasks::TaskRegistry;
use serde_json::Value;

fn open(root: &Path, session: &str) -> TaskRegistry {
    TaskRegistry::new_persistent(session.into(), root.into()).unwrap()
}

#[test]
fn resumed_task_is_protected_by_the_resuming_registry() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let creator = open(root, "creator");
    let runner = open(root, "runner");
    let task = creator.create_main_session("paused task".into());
    creator.mark_running(&task.id).unwrap();
    creator.request_pause(&task.id).unwrap();
    drop(creator);
    runner.request_resume(&task.id).unwrap();
    assert_eq!(
        open(root, "creator").get(&task.id).unwrap().status,
        TaskStatus::Running
    );
    drop(runner);
    assert_eq!(
        open(root, "creator").get(&task.id).unwrap().status,
        TaskStatus::Failed
    );
}

#[test]
fn cross_session_runner_protects_both_running_transitions_until_it_dies() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let creator = open(root, "creator");
    let runner = open(root, "runner");
    let main = creator.create_main_session("foreign main task".into());
    let child = creator.create_subagent("foreign leased task".into(), None);
    drop(creator);
    runner.mark_running(&main.id).unwrap();
    let lease = runner.acquire_task_lease(&child.id).unwrap();
    runner.mark_running_with_lease(&lease, &child.id).unwrap();
    let reopened = open(root, "creator");
    for id in [&main.id, &child.id] {
        assert_eq!(reopened.get(id).unwrap().status, TaskStatus::Running);
    }
    drop(runner);
    let recovered = open(root, "creator");
    for id in [&main.id, &child.id] {
        assert_eq!(recovered.get(id).unwrap().status, TaskStatus::Failed);
    }
}

#[test]
fn live_shell_survives_foreign_read_and_second_open_without_a_write() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let first = open(root, "shells");
    let shell = first.create_shell("shell".into(), "echo".into());
    first.mark_running(&shell.id).unwrap();
    let path = root.join("shells/tasks.json");
    let before = fs::read(&path).unwrap();
    for session in ["foreign", "shells"] {
        let reader = open(root, session);
        assert_eq!(reader.get(&shell.id).unwrap().status, TaskStatus::Running);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    let data: Value = serde_json::from_slice(&before).unwrap();
    let id = data[&shell.id]["registry_owner"]["id"].as_str().unwrap();
    let lock = root.join("shells/owners").join(format!("{id}.lock"));
    assert!(lock.is_file());
    drop(first);
    let recovered = open(root, "shells");
    assert_eq!(recovered.get(&shell.id).unwrap().status, TaskStatus::Failed);
    assert!(!lock.exists());
}

#[test]
fn ownerless_record_and_unknown_fields_remain_compatible() {
    let temp = tempfile::tempdir().unwrap();
    let registry = open(temp.path(), "legacy");
    let task = registry.create_main_session("older host".into());
    registry.mark_running(&task.id).unwrap();
    let path = temp.path().join("legacy/tasks.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let record = data[&task.id].as_object_mut().unwrap();
    assert!(record.remove("registry_owner").is_some());
    record.insert("unknown_field".into(), true.into());
    fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    let recovered = open(temp.path(), "legacy");
    assert_eq!(recovered.get(&task.id).unwrap().status, TaskStatus::Failed);
    let data: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(data[&task.id].get("registry_owner").is_none());
}

#[test]
fn crashed_owner_process_is_recovered_on_next_open() {
    if let Some(root) = std::env::var_os("ORCA_OWNER_CRASH_FIXTURE") {
        let registry = open(Path::new(&root), "crash");
        let shell = registry.create_shell("crash fixture".into(), "echo".into());
        registry.mark_running(&shell.id).unwrap();
        std::process::exit(0);
    }
    let temp = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crashed_owner_process_is_recovered_on_next_open",
            "--test-threads",
            "1",
        ])
        .env("ORCA_OWNER_CRASH_FIXTURE", temp.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let directory = temp.path().join("crash");
    let lock = fs::read_dir(directory.join("owners"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let data: Value =
        serde_json::from_slice(&fs::read(directory.join("tasks.json")).unwrap()).unwrap();
    let (id, record) = data.as_object().unwrap().iter().next().unwrap();
    assert_eq!(record["status"], "running");
    assert!(lock.exists());
    let recovered = open(temp.path(), "crash");
    assert_eq!(recovered.get(id).unwrap().status, TaskStatus::Failed);
    assert!(!lock.exists());
}
