use super::*;
use crate::tasks::TaskRegistry;

#[test]
fn unavailable_owner_disables_archiving_but_keeps_task_persistence() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let live = TaskRegistry::new_persistent("live".into(), root.into()).unwrap();
    let running = live.create_main_session("foreign".into());
    live.mark_running(&running.id).unwrap();
    let session = root.join("degraded");
    std::fs::create_dir(&session).unwrap();
    std::fs::write(session.join("owners"), b"not a directory").unwrap();
    let open = || TaskRegistry::new_persistent("degraded".into(), root.into()).unwrap();
    let tasks = open();
    assert!(tasks.supports_detached_subagent_relay());
    assert_eq!(
        tasks.get(&running.id).unwrap().status,
        orca_core::task_types::TaskStatus::Running
    );
    let task = tasks.create_shell("persist me".into(), "echo".into());
    tasks.mark_running(&task.id).unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session.join("tasks.json")).unwrap()).unwrap();
    assert!(persisted[&task.id].get("registry_owner").is_none());
    let error = TaskOutputStore::for_tasks(&tasks)
        .register_shell("shell", &task.id, false, false)
        .unwrap_err();
    assert!(error.to_string().contains("owner"), "{error}");
    assert!(!session.join("output").exists());
    drop(tasks);
    assert_eq!(
        open().get(&task.id).unwrap().status,
        orca_core::task_types::TaskStatus::Failed
    );
}

#[test]
fn legacy_read_ignores_held_owner_lock_and_never_updates_database_or_cursor() {
    let home = tempfile::tempdir().unwrap();
    let tasks = TaskRegistry::new_persistent("legacy".into(), home.path().into()).unwrap();
    let shell = tasks.create_shell("legacy".into(), "echo".into());
    tasks.mark_running(&shell.id).unwrap();
    let legacy = home.path().join("legacy/output");
    let writer = OutputArchive::open(&legacy, "legacy").unwrap();
    {
        let mut archive = writer.lock().unwrap();
        archive.register("held", &shell.id, false, false).unwrap();
        archive
            .append(&shell.id, TaskOutputStream::Stdout, "legacy")
            .unwrap();
    }
    let database = legacy.join("archive.sqlite3");
    let before = std::fs::read(&database).unwrap();
    let reader = TaskOutputStore::for_tasks(&tasks);
    assert_eq!(reader.archived_shell("held").unwrap().state, "running");
    tasks.fail(&shell.id, "host exited".into()).unwrap();
    assert_eq!(reader.archived_shell("held").unwrap().state, "interrupted");
    reader.set_cursor("held", 2).unwrap();
    assert_eq!(reader.archived_shell("held").unwrap().cursor, 2);
    assert_eq!(
        TaskOutputStore::for_tasks(&tasks)
            .archived_shell("held")
            .unwrap()
            .cursor,
        2
    );
    assert_eq!(
        reader.read_delta(&shell.id, 2, 10).unwrap().combined,
        "gacy"
    );
    assert_eq!(writer.lock().unwrap().shell("held").unwrap().cursor, 0);
    assert_eq!(
        writer.lock().unwrap().shell("held").unwrap().state,
        "running"
    );
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert!(!legacy.join("owners").exists());
    drop(reader);
    let reader = TaskOutputStore::for_tasks(&tasks);
    assert_eq!(reader.archived_shell("held").unwrap().cursor, 2);
    drop(writer);
    std::fs::remove_file(legacy.join("owner.lock")).unwrap();
    assert_eq!(reader.tail(&shell.id, 3).unwrap().combined, "acy");
    assert!(!legacy.join("owner.lock").exists());
    std::fs::write(database, b"not an archive").unwrap();
    assert_eq!(
        reader.archived_shell("held").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn sweep_caps_only_dead_archives_in_newest_task_sequence_order() {
    let home = tempfile::tempdir().unwrap();
    let open = || TaskRegistry::new_persistent("cap".into(), home.path().into()).unwrap();
    let create = |name: &str| {
        let tasks = open();
        let task = tasks.create_shell(name.into(), name.into());
        let store = TaskOutputStore::for_tasks(&tasks);
        store.register_shell(name, &task.id, false, false).unwrap();
        store.append_stdout(&task.id, "four").unwrap();
        (tasks.output_storage_root().unwrap().unwrap(), store)
    };
    let (live, live_store) = create("live");
    let (old, old_store) = create("old");
    let (new, new_store) = create("new");
    drop((old_store, new_store));
    let tasks = open();
    let route =
        routing::ArchiveRoute::new(tasks.clone(), tasks.output_storage_root().unwrap().unwrap());
    let _lock = tasks.output_session_lock().unwrap();
    route
        .sweep(persistence::ArchiveLimits {
            session_bytes: 4,
            ..Default::default()
        })
        .unwrap();
    assert!(new.is_dir());
    assert!(!old.exists());
    assert!(live.is_dir());
    drop(_lock);
    assert_eq!(live_store.archived_shell("live").unwrap().state, "running");
}

#[test]
fn registry_archives_are_lazy_isolated_and_recovered_after_owner_death() {
    let home = tempfile::tempdir().unwrap();
    let open = || TaskRegistry::new_persistent("shared".into(), home.path().into()).unwrap();
    let first = open();
    let first_store = TaskOutputStore::for_tasks(&first);
    let first_dir = first.output_storage_root().unwrap().unwrap();
    assert!(
        !first_dir.exists(),
        "a shell-less registry created an archive"
    );
    let live = first.create_shell("first".into(), "first".into());
    first_store
        .register_shell("first", &live.id, false, false)
        .unwrap();
    first_store.append_stdout(&live.id, "saved").unwrap();
    let second = open();
    let second_store = TaskOutputStore::for_tasks(&second);
    let second_dir = second.output_storage_root().unwrap().unwrap();
    assert_ne!(first_dir, second_dir);
    let abandoned = open();
    let abandoned_dir = abandoned.output_storage_root().unwrap().unwrap();
    drop(OutputArchive::open(&abandoned_dir, abandoned.session_id()).unwrap());
    std::fs::remove_file(abandoned_dir.join("archive.sqlite3")).unwrap();
    drop(abandoned);
    second_store
        .register_shell("unreferenced", "no-record", false, false)
        .unwrap();
    assert_eq!(
        second_store.archived_shell("first").unwrap().state,
        "running"
    );
    assert_eq!(
        second_store.read_delta(&live.id, 0, 99).unwrap().combined,
        "saved"
    );
    drop(second_store);
    drop(second);
    drop(first_store);
    drop(first);
    let third = open();
    let third_store = TaskOutputStore::for_tasks(&third);
    assert_eq!(
        third_store.archived_shell("first").unwrap().state,
        "interrupted"
    );
    third_store
        .register_shell("third", "third-task", false, false)
        .unwrap();
    assert!(first_dir.exists());
    assert!(!second_dir.exists());
    assert!(!abandoned_dir.exists());
    third_store.set_cursor("first", 2).unwrap();
    assert_eq!(third_store.archived_shell("first").unwrap().cursor, 2);
    assert_eq!(third_store.tail(&live.id, 3).unwrap().combined, "ved");
    assert!(!home.path().join("shared/output/owner.lock").exists());
}
