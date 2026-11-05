use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
const MAX_ENTRIES: usize = 8192;
const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules", ".orca"];
static OBSERVATION_WORKER_BUSY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static TEST_CAPTURE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Bounded, best-effort observation. A hash delta has no task attribution and
/// makes no claim about whether the requested work is correct.
#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    observed: bool,
    files: BTreeMap<String, String>,
    limitations: BTreeSet<&'static str>,
}
impl Snapshot {
    pub(crate) fn capture(root: &Path) -> Self {
        #[cfg(test)]
        let _serial = TEST_CAPTURE_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::capture_operation(root, |root| {
            let mut snapshot = Self {
                observed: true,
                ..Self::default()
            };
            // All filesystem operations, including root metadata and directory
            // enumeration, may block. Keep them off the operation's thread.
            #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
            unix_walk::capture(&mut snapshot, &root);
            #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
            {
                let _ = root;
                snapshot
                    .limitations
                    .insert("safe_directory_traversal_unavailable");
            }
            snapshot
        })
    }
    fn capture_operation(
        root: &Path,
        operation: impl FnOnce(std::path::PathBuf) -> Self + Send + 'static,
    ) -> Self {
        use std::sync::atomic::Ordering;
        struct WorkerGuard;
        impl Drop for WorkerGuard {
            fn drop(&mut self) {
                OBSERVATION_WORKER_BUSY.store(false, Ordering::Release);
            }
        }
        fn unavailable(reason: &'static str) -> Snapshot {
            Snapshot {
                observed: true,
                limitations: BTreeSet::from([reason]),
                ..Snapshot::default()
            }
        }
        // A regular-file read cannot be reliably cancelled. At most one
        // outstanding observer is allowed per process, so stalled backends
        // cannot accumulate detached threads or open directory handles.
        if OBSERVATION_WORKER_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return unavailable("observation_worker_busy");
        }
        let guard = WorkerGuard;
        let root = root.to_owned();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let started = std::time::Instant::now();
        if std::thread::Builder::new()
            .name("orca-workspace-observer".into())
            .spawn(move || {
                let snapshot = operation(root);
                drop(guard);
                let _ = sender.send(snapshot);
            })
            .is_err()
        {
            return unavailable("observation_worker_unavailable");
        }
        match receiver
            .recv_timeout(std::time::Duration::from_millis(250).saturating_sub(started.elapsed()))
        {
            Ok(snapshot) => snapshot,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => unavailable("observation_budget"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                unavailable("observation_worker_failed")
            }
        }
    }
    pub(crate) fn compare(&self, after: &Self) -> Value {
        let mut limitations: BTreeSet<_> = self
            .limitations
            .union(&after.limitations)
            .copied()
            .collect();
        if !self.observed || !after.observed {
            limitations.insert("not_observed");
        }
        let complete = limitations.is_empty();
        let mut changes = vec![];
        for path in self
            .files
            .keys()
            .chain(after.files.keys())
            .collect::<BTreeSet<_>>()
        {
            let before_hash = self.files.get(path);
            let after_hash = after.files.get(path);
            // A missing entry in a partial observation is not deletion evidence.
            if before_hash != after_hash
                && (complete || before_hash.is_some() && after_hash.is_some())
            {
                changes.push(
                    json!({"path": path, "before_sha256": before_hash, "after_sha256": after_hash}),
                );
            }
        }
        json!({"method": "bounded_filesystem_sha256", "attribution": "unknown",
            "changed": if !changes.is_empty() { Some(true) } else if complete { Some(false) } else { None },
            "complete": complete, "excluded_directories": EXCLUDED_DIRS,
            "limitations": limitations, "changes": changes})
    }
}

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
mod unix_walk {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::{CStr, CString, OsString};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, PathBuf};
    use std::time::{Duration, Instant};

    struct Directory(*mut libc::DIR);
    impl Directory {
        fn new(file: File) -> io::Result<Self> {
            let fd = file.into_raw_fd();
            // SAFETY: fd is owned, and fdopendir takes ownership on success.
            let dir = unsafe { libc::fdopendir(fd) };
            if dir.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: fdopendir did not consume fd on failure.
                drop(unsafe { File::from_raw_fd(fd) });
                return Err(error);
            }
            Ok(Self(dir))
        }
        fn fd(&self) -> RawFd {
            // SAFETY: self owns a live DIR until Drop.
            unsafe { libc::dirfd(self.0) }
        }
        fn next(&mut self) -> io::Result<Option<OsString>> {
            // SAFETY: errno is thread-local; no pointer survives readdir.
            unsafe {
                #[cfg(target_vendor = "apple")]
                let errno = libc::__error();
                #[cfg(target_os = "linux")]
                let errno = libc::__errno_location();
                #[cfg(target_os = "android")]
                let errno = libc::__errno();
                *errno = 0;
                let entry = libc::readdir(self.0);
                if entry.is_null() {
                    return if *errno == 0 {
                        Ok(None)
                    } else {
                        Err(io::Error::from_raw_os_error(*errno))
                    };
                }
                Ok(Some(OsString::from_vec(
                    CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes().to_vec(),
                )))
            }
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this is the sole owner of the DIR and its fd.
            unsafe { libc::closedir(self.0) };
        }
    }

    fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: stat points to space for a stat, initialized on success.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }
    fn stat_at(fd: RawFd, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: name is terminated and stat is initialized on success.
        if unsafe {
            libc::fstatat(
                fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }
    fn same_identity(a: &libc::stat, b: &libc::stat) -> bool {
        a.st_dev == b.st_dev && a.st_ino == b.st_ino && a.st_mode == b.st_mode
    }
    fn unchanged(a: &libc::stat, b: &libc::stat) -> bool {
        same_identity(a, b)
            && a.st_size == b.st_size
            && a.st_mtime == b.st_mtime
            && a.st_mtime_nsec == b.st_mtime_nsec
            && a.st_ctime == b.st_ctime
            && a.st_ctime_nsec == b.st_ctime_nsec
    }
    fn open_at(fd: RawFd, name: &CStr, directory: bool) -> io::Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: name is terminated; no mode is needed without O_CREAT.
        let opened = unsafe { libc::openat(fd, name.as_ptr(), flags) };
        if opened < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned fd.
        Ok(unsafe { File::from_raw_fd(opened) })
    }
    fn open_checked(
        fd: RawFd,
        name: &CStr,
        expected: &libc::stat,
        directory: bool,
    ) -> io::Result<File> {
        let file = open_at(fd, name, directory)?;
        if !unchanged(expected, &stat_fd(file.as_raw_fd())?) {
            return Err(io::Error::other("workspace entry changed while opening"));
        }
        Ok(file)
    }

    pub(super) fn capture(snapshot: &mut Snapshot, root: &Path) {
        let started = Instant::now();
        let result = (|| -> io::Result<()> {
            let expected = std::fs::symlink_metadata(root)?;
            if expected.is_symlink() {
                snapshot.limitations.insert("symlink_excluded");
                return Ok(());
            }
            if !expected.is_dir() {
                return Err(io::Error::other("workspace root is not a directory"));
            }
            // Canonicalize only the configured root (e.g. macOS /var). Then
            // open every component with no-follow; verify the original root's
            // identity before reading anything from its directory handle.
            let canonical = root.canonicalize()?;
            let mut file = open_at(libc::AT_FDCWD, c"/", true)?;
            for component in canonical.components() {
                if started.elapsed() > Duration::from_millis(250) {
                    snapshot.limitations.insert("observation_budget");
                    return Ok(());
                }
                if let Component::Normal(name) = component {
                    let name = CString::new(name.as_bytes())?;
                    let before = stat_at(file.as_raw_fd(), &name)?;
                    file = open_checked(file.as_raw_fd(), &name, &before, true)?;
                }
            }
            let before = stat_fd(file.as_raw_fd())?;
            if expected.dev() != before.st_dev as u64 || expected.ino() != before.st_ino as u64 {
                snapshot.limitations.insert("concurrent_change");
                return Ok(());
            }
            let mut scan = Scan {
                snapshot,
                started,
                bytes: 0,
                entries: 0,
            };
            scan.walk(Directory::new(file)?, Path::new(""), 0)?;
            let after = std::fs::symlink_metadata(root)?;
            if after.is_symlink() || after.dev() != expected.dev() || after.ino() != expected.ino()
            {
                scan.snapshot.files.clear();
                scan.snapshot.limitations.insert("concurrent_change");
            }
            Ok(())
        })();
        if result.is_err() {
            // The root may have disappeared after scanning it.
            snapshot.files.clear();
            snapshot
                .limitations
                .insert("read_error_or_concurrent_change");
        }
    }

    struct Scan<'a> {
        snapshot: &'a mut Snapshot,
        started: Instant,
        bytes: u64,
        entries: usize,
    }
    impl Scan<'_> {
        fn budget_exhausted(&mut self) -> bool {
            let exhausted = self.entries >= MAX_ENTRIES
                || self.bytes >= MAX_TOTAL_BYTES
                || self.started.elapsed() > Duration::from_millis(250);
            if exhausted {
                self.snapshot.limitations.insert("observation_budget");
            }
            exhausted
        }
        fn discard_subtree(&mut self, relative: &Path) {
            self.snapshot
                .files
                .retain(|path, _| !Path::new(path).starts_with(relative));
            self.snapshot.limitations.insert("concurrent_change");
        }
        fn walk(&mut self, mut dir: Directory, relative: &Path, depth: usize) -> io::Result<()> {
            if depth > 32 {
                self.snapshot.limitations.insert("depth_limit");
                return Ok(());
            }
            let before = stat_fd(dir.fd())?;
            while !self.budget_exhausted() {
                let Some(name) = dir.next()? else { break };
                if name == "." || name == ".." {
                    continue;
                }
                self.entries += 1;
                let path: PathBuf = relative.join(&name);
                let name_c = CString::new(name.as_bytes())?;
                let result = self.entry(dir.fd(), &name_c, &name, &path, depth);
                if result.is_err() {
                    self.snapshot
                        .limitations
                        .insert("read_error_or_concurrent_change");
                }
            }
            if !unchanged(&before, &stat_fd(dir.fd())?) {
                self.discard_subtree(relative);
            }
            Ok(())
        }
        fn entry(
            &mut self,
            parent: RawFd,
            name: &CStr,
            display_name: &OsString,
            relative: &Path,
            depth: usize,
        ) -> io::Result<()> {
            let before = stat_at(parent, name)?;
            let kind = before.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                self.snapshot.limitations.insert("symlink_excluded");
                return Ok(());
            }
            if kind == libc::S_IFDIR {
                if EXCLUDED_DIRS
                    .iter()
                    .any(|excluded| display_name == *excluded)
                {
                    return Ok(());
                }
                let file = open_checked(parent, name, &before, true)?;
                let result = self.walk(Directory::new(file)?, relative, depth + 1);
                if result.is_err()
                    || !stat_at(parent, name).is_ok_and(|after| unchanged(&before, &after))
                {
                    self.discard_subtree(relative);
                }
                return result;
            }
            if kind != libc::S_IFREG {
                self.snapshot.limitations.insert("special_file_excluded");
                return Ok(());
            }
            if before.st_size < 0 || before.st_size as u64 > MAX_FILE_BYTES {
                self.snapshot.limitations.insert("file_size_limit");
                return Ok(());
            }
            if self.bytes.saturating_add(before.st_size as u64) > MAX_TOTAL_BYTES {
                self.snapshot.limitations.insert("observation_budget");
                // Stop the whole traversal after exceeding the byte budget.
                self.bytes = MAX_TOTAL_BYTES;
                return Ok(());
            }
            let Some(path) = relative.to_str() else {
                self.snapshot.limitations.insert("non_utf8_path");
                return Ok(());
            };
            let mut file = open_checked(parent, name, &before, false)?;
            let mut data = Vec::new();
            let result = (&mut file).take(MAX_FILE_BYTES + 1).read_to_end(&mut data);
            // Failed/racing reads also consume the observation budget.
            self.bytes = self.bytes.saturating_add(data.len() as u64);
            result?;
            if data.len() as u64 != before.st_size as u64
                || !unchanged(&before, &stat_fd(file.as_raw_fd())?)
                || !unchanged(&before, &stat_at(parent, name)?)
            {
                self.snapshot.limitations.insert("concurrent_change");
                return Ok(());
            }
            self.snapshot.files.insert(
                path.into(),
                Sha256::digest(data)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            );
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn rejects_directory_replacement_between_stat_and_open() {
            let workspace = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let nested = workspace.path().join("nested");
            std::fs::create_dir(&nested).unwrap();
            std::fs::write(outside.path().join("outside-only"), "outside").unwrap();
            let parent = File::open(workspace.path()).unwrap();
            let before = stat_at(parent.as_raw_fd(), c"nested").unwrap();
            std::fs::rename(&nested, outside.path().join("old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), &nested).unwrap();
            assert!(open_checked(parent.as_raw_fd(), c"nested", &before, true).is_err());
            let snapshot = Snapshot::capture(workspace.path());
            assert!(snapshot.files.is_empty());
            assert!(!snapshot.limitations.is_empty());
        }
        #[test]
        fn pinned_directory_handle_does_not_follow_replaced_ancestor() {
            let workspace = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let nested = workspace.path().join("nested");
            std::fs::create_dir(&nested).unwrap();
            std::fs::write(nested.join("inside"), "inside").unwrap();
            std::fs::write(outside.path().join("outside-only"), "outside").unwrap();
            let parent = File::open(workspace.path()).unwrap();
            let before = stat_at(parent.as_raw_fd(), c"nested").unwrap();
            let pinned = open_checked(parent.as_raw_fd(), c"nested", &before, true).unwrap();
            std::fs::rename(&nested, outside.path().join("old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), &nested).unwrap();
            let mut snapshot = Snapshot::default();
            let mut scan = Scan {
                snapshot: &mut snapshot,
                started: Instant::now(),
                bytes: 0,
                entries: 0,
            };
            scan.walk(Directory::new(pinned).unwrap(), Path::new("nested"), 1)
                .unwrap();
            assert!(!scan.snapshot.files.contains_key("nested/outside-only"));
            // The parent-entry check discards any hashes from the moved tree.
            assert!(!unchanged(
                &before,
                &stat_at(parent.as_raw_fd(), c"nested").unwrap()
            ));
            scan.discard_subtree(Path::new("nested"));
            assert!(scan.snapshot.files.is_empty());
            assert!(scan.snapshot.limitations.contains("concurrent_change"));
        }

        #[test]
        fn rejects_directory_identity_replacement() {
            let workspace = tempfile::tempdir().unwrap();
            let nested = workspace.path().join("nested");
            std::fs::create_dir(&nested).unwrap();
            let parent = File::open(workspace.path()).unwrap();
            let before = stat_at(parent.as_raw_fd(), c"nested").unwrap();
            std::fs::rename(&nested, workspace.path().join("old")).unwrap();
            std::fs::create_dir(&nested).unwrap();
            assert!(open_checked(parent.as_raw_fd(), c"nested", &before, true).is_err());
        }

        #[test]
        fn rejects_same_size_file_replacement_between_stat_and_open() {
            let workspace = tempfile::tempdir().unwrap();
            let path = workspace.path().join("file");
            std::fs::write(&path, "before").unwrap();
            let parent = File::open(workspace.path()).unwrap();
            let before = stat_at(parent.as_raw_fd(), c"file").unwrap();
            std::fs::rename(&path, workspace.path().join("old")).unwrap();
            std::fs::write(&path, "after!").unwrap();
            assert!(open_checked(parent.as_raw_fd(), c"file", &before, false).is_err());
        }
    }
}

#[cfg(all(
    test,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
mod tests {
    use super::*;
    #[test]
    fn blocked_observation_returns_unknown_within_deadline_and_limits_workers() {
        // Cargo tests share a process. Serialize against every real capture,
        // including captures performed by other runtime tests.
        let _serial = TEST_CAPTURE_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let releaser = std::thread::spawn(move || {
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(650));
            let _ = release_tx.send(());
        });
        let snapshot = Snapshot::capture_operation(Path::new("unused"), move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Snapshot {
                observed: true,
                ..Snapshot::default()
            }
        });
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        let evidence = snapshot.compare(&snapshot);
        assert_eq!(evidence["complete"], false);
        assert_eq!(evidence["changed"], Value::Null);
        assert!(snapshot.limitations.contains("observation_budget"));
        let other = Snapshot::capture_operation(Path::new("unused"), |_| {
            panic!("must not start a second blocked worker")
        });
        assert!(other.limitations.contains("observation_worker_busy"));
        releaser.join().unwrap();
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while OBSERVATION_WORKER_BUSY.load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < cleanup_deadline
        {
            std::thread::yield_now();
        }
        assert!(!OBSERVATION_WORKER_BUSY.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn observes_shell_changes_without_claiming_authority_or_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "before").unwrap();
        let before = Snapshot::capture(dir.path());
        std::fs::write(dir.path().join("a"), "after").unwrap();
        std::fs::write(dir.path().join("b"), "new").unwrap();
        let evidence = before.compare(&Snapshot::capture(dir.path()));
        assert_eq!(evidence["changed"], true);
        assert_eq!(evidence["attribution"], "unknown");
        assert_eq!(evidence["changes"].as_array().unwrap().len(), 2);
        assert_eq!(evidence["changes"][0]["path"], "a");
        assert_ne!(
            evidence["changes"][0]["before_sha256"],
            evidence["changes"][0]["after_sha256"]
        );
    }
    #[test]
    fn observes_nested_files_and_excludes_build_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nested/deeper")).unwrap();
        std::fs::write(dir.path().join("nested/deeper/file"), "before").unwrap();
        for excluded in EXCLUDED_DIRS {
            std::fs::create_dir(dir.path().join(excluded)).unwrap();
            std::fs::write(dir.path().join(excluded).join("ignored"), "before").unwrap();
        }
        let before = Snapshot::capture(dir.path());
        std::fs::write(dir.path().join("nested/deeper/file"), "after").unwrap();
        for excluded in EXCLUDED_DIRS {
            std::fs::write(dir.path().join(excluded).join("ignored"), "after").unwrap();
        }
        let evidence = before.compare(&Snapshot::capture(dir.path()));
        assert_eq!(evidence["complete"], true);
        assert_eq!(evidence["changes"].as_array().unwrap().len(), 1);
        assert_eq!(evidence["changes"][0]["path"], "nested/deeper/file");
    }

    #[test]
    fn restoration_is_no_net_change_even_when_a_write_occurred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "dirty baseline").unwrap();
        let before = Snapshot::capture(dir.path());
        std::fs::write(dir.path().join("a"), "temporary edit").unwrap();
        std::fs::write(dir.path().join("a"), "dirty baseline").unwrap();
        assert_eq!(
            before.compare(&Snapshot::capture(dir.path()))["changed"],
            false
        );
    }

    #[test]
    fn omitted_large_files_make_negative_result_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("large"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let before = Snapshot::capture(dir.path());
        let evidence = before.compare(&Snapshot::capture(dir.path()));
        assert_eq!(evidence["complete"], false);
        assert!(evidence["changed"].is_null());
        assert!(
            evidence["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x == "file_size_limit")
        );
    }
    #[cfg(target_vendor = "apple")]
    #[test]
    fn directory_symlink_swap_never_observes_outside_files() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = workspace.path().join("nested");
        let spare = outside.path().join("spare");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(outside.path().join("outside-only"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), &spare).unwrap();
        let before = Snapshot::capture(workspace.path());
        assert!(before.limitations.is_empty());
        let nested = std::ffi::CString::new(nested.as_os_str().as_encoded_bytes()).unwrap();
        let spare = std::ffi::CString::new(spare.as_os_str().as_encoded_bytes()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let control = Arc::clone(&stop);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let worker = std::thread::spawn(move || {
            let mut swaps = 0;
            while !control.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                // SAFETY: both paths exist and CString keeps them terminated.
                assert_eq!(
                    unsafe { libc::renamex_np(nested.as_ptr(), spare.as_ptr(), libc::RENAME_SWAP) },
                    0
                );
                swaps += 1;
            }
            if swaps % 2 != 0 {
                assert_eq!(
                    unsafe { libc::renamex_np(nested.as_ptr(), spare.as_ptr(), libc::RENAME_SWAP) },
                    0
                );
            }
            swaps
        });
        // Bounded stress regression; deterministic stat/open tests also cover
        // the critical race window without relying on scheduling.
        let mut outside_observed = false;
        for _ in 0..5000 {
            if std::time::Instant::now() >= deadline {
                break;
            }
            let after = Snapshot::capture(workspace.path());
            if !after.files.is_empty() || before.compare(&after)["changed"] == true {
                outside_observed = true;
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap() > 0);
        assert!(
            !outside_observed,
            "directory replacement produced outside file evidence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_a_symlink_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside-only"), "outside").unwrap();
        let root = dir.path().join("root");
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let snapshot = Snapshot::capture(&root);
        assert!(
            snapshot.files.is_empty(),
            "symlink root escaped the workspace"
        );
        assert!(!snapshot.limitations.is_empty());
    }

    #[test]
    fn partial_snapshots_do_not_report_unobserved_additions_or_deletions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("removed"), "before").unwrap();
        let before = Snapshot::capture(dir.path());
        std::fs::remove_file(dir.path().join("removed")).unwrap();
        std::fs::write(dir.path().join("added"), "after").unwrap();
        std::fs::File::create(dir.path().join("large"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let evidence = before.compare(&Snapshot::capture(dir.path()));
        assert_eq!(evidence["complete"], false);
        assert!(evidence["changed"].is_null());
        assert!(evidence["changes"].as_array().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symlinks_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let snapshot = Snapshot::capture(dir.path());
        assert!(snapshot.files.is_empty());
        assert!(snapshot.limitations.contains("symlink_excluded"));
    }
}

#[cfg(all(
    test,
    not(any(target_vendor = "apple", target_os = "linux", target_os = "android"))
))]
mod fallback_tests {
    use super::*;
    #[test]
    fn unsupported_traversal_is_incomplete_without_file_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "before").unwrap();
        let before = Snapshot::capture(dir.path());
        std::fs::write(dir.path().join("file"), "after").unwrap();
        let evidence = before.compare(&Snapshot::capture(dir.path()));
        assert_eq!(evidence["complete"], false);
        assert!(evidence["changed"].is_null());
        assert!(evidence["changes"].as_array().unwrap().is_empty());
        assert!(
            before
                .limitations
                .contains("safe_directory_traversal_unavailable")
        );
    }
}
