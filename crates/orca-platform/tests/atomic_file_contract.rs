use std::io::{self, Write};
use std::path::Path;

#[cfg(windows)]
use orca_platform::fs::ExclusiveFileLock;
#[cfg(unix)]
use orca_platform::fs::open_nofollow_nonblocking;
use orca_platform::fs::{AtomicWritePolicy, atomic_write, atomic_write_with, open_nofollow};

#[test]
fn atomic_replace_never_leaves_a_partial_file_or_temp_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");

    atomic_write(&path, br#"{"revision":1}"#, AtomicWritePolicy::NoFollow).expect("first write");
    atomic_write(&path, br#"{"revision":2}"#, AtomicWritePolicy::NoFollow).expect("replace");

    assert_eq!(std::fs::read(&path).expect("read"), br#"{"revision":2}"#);
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn atomic_write_with_streams_and_replaces_the_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("transcript.jsonl");
    std::fs::write(&path, b"old").expect("old content");

    atomic_write_with(&path, AtomicWritePolicy::NoFollow, |file| {
        file.write_all(b"first\n")?;
        file.write_all(b"second\n")
    })
    .expect("streamed replace");

    assert_eq!(std::fs::read(&path).expect("read"), b"first\nsecond\n");
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn failed_atomic_write_with_keeps_the_old_destination_and_cleans_the_temp_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("transcript.jsonl");
    std::fs::write(&path, b"old").expect("old content");

    let error = atomic_write_with(&path, AtomicWritePolicy::NoFollow, |file| {
        file.write_all(b"partial")?;
        Err(io::Error::other("injected writer failure"))
    })
    .expect_err("writer failure");

    assert!(matches!(
        error,
        orca_platform::PlatformError::Io {
            kind: io::ErrorKind::Other,
            ..
        }
    ));
    assert_eq!(std::fs::read(&path).expect("old destination"), b"old");
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn failed_replace_keeps_the_old_destination_and_cleans_the_temp_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let destination = temp.path().join("state.json");
    std::fs::create_dir(&destination).expect("directory collision");
    std::fs::write(destination.join("keep"), b"old").expect("old content");

    assert!(atomic_write(&destination, b"new", AtomicWritePolicy::NoFollow).is_err());
    assert_eq!(
        std::fs::read(destination.join("keep")).expect("old destination survives"),
        b"old"
    );
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn no_follow_rejects_symlink_destinations_and_opening_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target.json");
    let link = temp.path().join("link.json");
    std::fs::write(&target, b"old").expect("target");
    symlink(&target, &link).expect("symlink");

    assert!(atomic_write(&link, b"new", AtomicWritePolicy::NoFollow).is_err());
    assert!(open_nofollow(&link).is_err());
    assert_eq!(std::fs::read(&target).expect("target remains"), b"old");
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn replace_destination_replaces_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target.json");
    let link = temp.path().join("link.json");
    std::fs::write(&target, b"target").expect("target");
    symlink(&target, &link).expect("symlink");

    atomic_write(&link, b"replacement", AtomicWritePolicy::ReplaceDestination)
        .expect("replace symlink directory entry");

    assert_eq!(std::fs::read(&link).expect("replacement"), b"replacement");
    assert_eq!(std::fs::read(&target).expect("target remains"), b"target");
    assert!(
        !std::fs::symlink_metadata(&link)
            .expect("replacement metadata")
            .file_type()
            .is_symlink()
    );
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn replacement_preserves_existing_unix_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("private.json");
    std::fs::write(&path, b"old").expect("old file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .expect("set permissions");

    atomic_write(&path, b"new", AtomicWritePolicy::NoFollow).expect("replace");

    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
}

#[test]
fn no_follow_opens_a_regular_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("regular.txt");
    std::fs::write(&path, b"content").expect("regular file");

    let file = open_nofollow(&path).expect("open regular file");
    assert_eq!(file.metadata().expect("metadata").len(), 7);
}

#[cfg(unix)]
#[test]
fn no_follow_nonblocking_rejects_fifo_without_waiting_for_a_writer() {
    use std::sync::mpsc;
    use std::time::Duration;

    let temp = tempfile::tempdir().expect("tempdir");
    let fifo = temp.path().join("named-pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success());

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(open_nofollow_nonblocking(&fifo));
    });

    let file = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("nonblocking open must not wait for a FIFO writer")
        .expect("open FIFO without following links");
    assert!(!file.metadata().expect("FIFO metadata").is_file());
}

#[cfg(windows)]
#[test]
fn no_follow_atomic_write_rejects_a_directory_junction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let junction = temp.path().join("junction");
    std::fs::create_dir(&target).expect("junction target");

    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .expect("invoke mklink /J");
    assert!(
        output.status.success(),
        "mklink /J failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for policy in [
        AtomicWritePolicy::NoFollow,
        AtomicWritePolicy::ReplaceDestination,
    ] {
        let error = atomic_write(&junction, b"new", policy).expect_err("junction must be rejected");
        assert!(matches!(
            error,
            orca_platform::PlatformError::ReparsePointRejected { .. }
        ));
    }
    assert_no_temp_artifacts(temp.path());
}

#[cfg(windows)]
#[test]
fn concurrent_atomic_writers_complete_and_leave_a_readable_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");
    atomic_write(&path, b"seed", AtomicWritePolicy::NoFollow).expect("seed state");

    std::thread::scope(|scope| {
        for writer in 0..4 {
            let path = path.clone();
            scope.spawn(move || {
                for revision in 0..64 {
                    let value = format!("writer-{writer}-revision-{revision}");
                    atomic_write(&path, value.as_bytes(), AtomicWritePolicy::NoFollow)
                        .expect("concurrent atomic write");
                }
            });
        }
        for _ in 0..4 {
            let path = path.clone();
            scope.spawn(move || {
                for _ in 0..512 {
                    match std::fs::read_to_string(&path) {
                        Ok(value) => assert!(!value.is_empty()),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {}
                        Err(error) => panic!("concurrent atomic read failed: {error}"),
                    }
                }
            });
        }
    });

    assert!(
        !std::fs::read_to_string(&path)
            .expect("final atomic destination")
            .is_empty()
    );
    assert_no_temp_artifacts(temp.path());
}

#[cfg(windows)]
#[test]
fn cross_process_atomic_writer_child() {
    let Some(destination) = std::env::var_os("ORCA_ATOMIC_WRITE_CHILD_DESTINATION") else {
        return;
    };
    let start =
        std::env::var_os("ORCA_ATOMIC_WRITE_CHILD_START").expect("cross-process writer start path");
    let lock =
        std::env::var_os("ORCA_ATOMIC_WRITE_CHILD_LOCK").expect("cross-process writer lock path");
    let ready =
        std::env::var_os("ORCA_ATOMIC_WRITE_CHILD_READY").expect("cross-process writer ready path");
    let writer = std::env::var("ORCA_ATOMIC_WRITE_CHILD_ID").expect("cross-process writer id");

    std::fs::write(&ready, b"ready").expect("announce cross-process writer");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !Path::new(&start).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "cross-process writer start signal timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let destination = Path::new(&destination);
    for revision in 0..128 {
        let value = format!("writer-{writer}-revision-{revision}");
        let _guard = ExclusiveFileLock::acquire(Path::new(&lock))
            .expect("acquire cross-process atomic write lock");
        atomic_write(destination, value.as_bytes(), AtomicWritePolicy::NoFollow)
            .expect("cross-process atomic write");
    }
}

#[cfg(windows)]
#[test]
fn concurrent_cross_process_atomic_writers_are_serialized() {
    let temp = tempfile::tempdir().expect("tempdir");
    let destination = temp.path().join("state.json");
    let lock = temp.path().join("state.lock");
    let start = temp.path().join("start");
    atomic_write(&destination, b"seed", AtomicWritePolicy::NoFollow).expect("seed state");

    let executable = std::env::current_exe().expect("current test executable");
    let mut children = Vec::new();
    let mut ready_paths = Vec::new();
    for writer in 0..4 {
        let ready = temp.path().join(format!("writer-{writer}.ready"));
        let child = std::process::Command::new(&executable)
            .args([
                "--exact",
                "cross_process_atomic_writer_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("ORCA_ATOMIC_WRITE_CHILD_DESTINATION", &destination)
            .env("ORCA_ATOMIC_WRITE_CHILD_START", &start)
            .env("ORCA_ATOMIC_WRITE_CHILD_LOCK", &lock)
            .env("ORCA_ATOMIC_WRITE_CHILD_READY", &ready)
            .env("ORCA_ATOMIC_WRITE_CHILD_ID", writer.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("launch cross-process atomic writer");
        children.push(child);
        ready_paths.push(ready);
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ready_paths.iter().any(|ready| !ready.exists()) {
        if std::time::Instant::now() >= deadline {
            for child in &mut children {
                let _ = child.kill();
            }
            for mut child in children {
                let _ = child.wait();
            }
            panic!("cross-process writers did not become ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::write(&start, b"go").expect("release cross-process writers");

    let outputs = children
        .into_iter()
        .map(|child| {
            child
                .wait_with_output()
                .expect("wait for cross-process atomic writer")
        })
        .collect::<Vec<_>>();
    for child in outputs {
        assert!(
            child.status.success(),
            "cross-process atomic writer failed: status={:?}, stdout={}, stderr={}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }
    assert!(
        std::fs::read_to_string(&destination)
            .expect("final atomic destination")
            .starts_with("writer-")
    );
    assert_no_temp_artifacts(temp.path());
}

fn assert_no_temp_artifacts(directory: &Path) {
    let artifacts = directory
        .read_dir()
        .expect("entries")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".orca-") && name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(artifacts.is_empty(), "temp artifacts: {artifacts:?}");
}
