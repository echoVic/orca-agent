//! The one place that decides where Orca keeps its own files.
//!
//! The Orca home holds configuration, folder trust, credentials, sessions,
//! task sessions, goals, memory and caches. It is `ORCA_HOME` when that is
//! set and `~/.orca` otherwise; project-local files never influence it.
//! Everything that reads or writes under it resolves the path with
//! [`orca_home`].
//!
//! Test builds never touch the real `~/.orca`. When no override and no
//! `ORCA_HOME` names a home, [`orca_home`] returns a temporary directory that
//! is private to the process; these live in `<temp dir>/orca-th`, so
//! one `rm -rf` clears what test runs leave behind, and each process that
//! creates one there removes those last changed more than a day ago. A test
//! build is this crate's own `cfg(test)`, or any build that enables the
//! `test-utils` feature while cargo or nextest is running it (see
//! `test_utils_default_home`). Other crates enable the feature through a
//! dev-dependency, so release builds keep the `~/.orca` default.

use std::cell::RefCell;
use std::path::PathBuf;

/// The environment variable that names the Orca home.
pub const ORCA_HOME_ENV: &str = "ORCA_HOME";

// Test-support override machinery. The orca-runtime test harness installs
// per-thread home overrides here instead of mutating the environment (the
// process-wide `ORCA_HOME` stays on the isolated test home), so a test and the
// hosts it spawns resolve a private home while every other test keeps
// resolving the process-wide one. Production code never installs these.
thread_local! {
    static TEST_ORCA_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static HOST_ORCA_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[doc(hidden)]
pub fn install_test_orca_home(home: Option<PathBuf>) {
    TEST_ORCA_HOME.with(|cell| *cell.borrow_mut() = home);
}

#[doc(hidden)]
pub fn install_host_orca_home(home: Option<PathBuf>) {
    HOST_ORCA_HOME.with(|cell| *cell.borrow_mut() = home);
}

#[doc(hidden)]
pub fn current_test_orca_home() -> Option<PathBuf> {
    TEST_ORCA_HOME.with(|cell| cell.borrow().clone())
}

/// The effective per-thread override (host override first, then test
/// override), as consulted by [`orca_home`] and the orca-runtime test harness.
#[doc(hidden)]
pub fn current_orca_home_override() -> Option<PathBuf> {
    HOST_ORCA_HOME
        .with(|cell| cell.borrow().clone())
        .or_else(current_test_orca_home)
}

/// Resolve the user-owned Orca home without creating it: the per-thread
/// override when one is installed, else `ORCA_HOME`, else `~/.orca`. In a test
/// build the last step is the process's temporary home instead.
pub fn orca_home() -> Option<PathBuf> {
    current_orca_home_override()
        .or_else(home_from_environment)
        .or_else(default_orca_home)
}

/// `ORCA_HOME`, unless it is unset or blank: a blank value would name the
/// working directory, which project files control.
fn home_from_environment() -> Option<PathBuf> {
    let value = std::env::var_os(ORCA_HOME_ENV)?;
    if value.to_str().is_some_and(|text| text.trim().is_empty()) {
        return None;
    }
    Some(PathBuf::from(value))
}

fn user_default_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".orca"))
}

#[cfg(not(any(test, feature = "test-utils")))]
fn default_orca_home() -> Option<PathBuf> {
    user_default_home()
}

/// orca-core's own unit tests always get the process's temporary home.
#[cfg(test)]
fn default_orca_home() -> Option<PathBuf> {
    Some(adopt_process_temp_home())
}

#[cfg(all(feature = "test-utils", not(test)))]
fn default_orca_home() -> Option<PathBuf> {
    test_utils_default_home()
}

/// Set by cargo and nextest for every test process they launch, and inherited
/// by whatever those processes spawn.
#[cfg(any(test, feature = "test-utils"))]
const CARGO_MANIFEST_DIR_ENV: &str = "CARGO_MANIFEST_DIR";

/// What a `test-utils` build falls back to when nothing names a home: the
/// process's temporary home, but only while cargo or nextest is running it.
///
/// Dev-dependencies switch the feature on, so the `orca` binary that the
/// integration tests run carries the fallback and stays in `target/` after
/// `cargo test`. Whoever starts that binary from a terminal must still get the
/// real `~/.orca`: a fresh temporary home on every run would hide their
/// config, API key and first-run acknowledgement. `CARGO_MANIFEST_DIR` marks a
/// test run (it reaches the test processes and the `orca` they spawn) and is
/// absent in a terminal. This only resolves a path in the user's home; it
/// creates and writes nothing there.
#[cfg(any(test, feature = "test-utils"))]
fn test_utils_default_home() -> Option<PathBuf> {
    if std::env::var_os(CARGO_MANIFEST_DIR_ENV).is_some() {
        Some(adopt_process_temp_home())
    } else {
        user_default_home()
    }
}

/// Makes the process's temporary home this process's `ORCA_HOME`, so child
/// processes land in the same directory rather than creating their own.
#[cfg(any(test, feature = "test-utils"))]
fn adopt_process_temp_home() -> PathBuf {
    let home = process_temp_home();
    // edition 2024: set_var is unsafe. Only reached while `ORCA_HOME` names
    // no home, and always with the same value.
    unsafe {
        std::env::set_var(ORCA_HOME_ENV, home);
    }
    home.to_path_buf()
}

/// The temporary home every test build in this process falls back to. It is
/// created on first use, inside `<temp dir>/orca-th`, and this process
/// never removes it: a detached child may still be writing to it when this one
/// exits. Instead, the first use in each process removes some of the homes
/// there, and in the legacy group, that were last changed more than a day ago
/// (see `prune_stale_test_homes`). This function does not touch the
/// environment.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn process_temp_home() -> &'static std::path::Path {
    use std::sync::OnceLock;

    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let mut builder = tempfile::Builder::new();
        builder.prefix(TEST_HOME_PREFIX);
        let group = std::env::temp_dir().join(TEST_HOME_GROUP);
        std::fs::create_dir_all(&group)
            .and_then(|()| builder.tempdir_in(&group))
            .inspect(|_| {
                prune_stale_test_homes(&group, TEST_HOME_PREFIX);
                // No process makes homes in the legacy group any more: once
                // its last one is pruned, it goes too.
                let legacy = std::env::temp_dir().join(LEGACY_TEST_HOME_GROUP);
                prune_stale_test_homes(&legacy, LEGACY_TEST_HOME_PREFIX);
                let _ = std::fs::remove_dir(legacy);
            })
            // Grouping is a convenience: on a temp dir shared between users
            // the group may belong to someone else, and the home must still
            // be created.
            .or_else(|_| builder.tempdir())
            .expect("create the process-wide test Orca home")
    })
    .path()
}

/// The directory, inside the temp dir, that holds every temporary home.
///
/// It and [`TEST_HOME_PREFIX`] are short on purpose. A session's image asset
/// sits about 190 characters below the home it is written in, and on Windows
/// tempfile publishes a file with `MoveFileExW`, which takes no
/// extended-length path and fails past `MAX_PATH` (260): with the GitHub
/// runner's 36-character temp dir, a longer group and prefix pushed the asset
/// tests' paths over it.
#[cfg(any(test, feature = "test-utils"))]
const TEST_HOME_GROUP: &str = "orca-th";

/// What each temporary home's name starts with.
#[cfg(any(test, feature = "test-utils"))]
const TEST_HOME_PREFIX: &str = "h-";

/// The group, and the name prefix, the temporary homes had before e51e7a52.
#[cfg(any(test, feature = "test-utils"))]
const LEGACY_TEST_HOME_GROUP: &str = "orca-test-homes";
#[cfg(any(test, feature = "test-utils"))]
const LEGACY_TEST_HOME_PREFIX: &str = "orca-test-home-";

/// How many stale homes one process removes from a group at most. After a
/// day without tests a group can hold thousands, and removing them all held
/// up the process's first test for seconds; the processes after it take the
/// rest.
#[cfg(any(test, feature = "test-utils"))]
const MAX_PRUNED_TEST_HOMES: usize = 32;

/// How long ago a temporary home must have last changed for another process
/// to remove it: far longer than any test run, and than any child it leaves
/// behind still writing there.
#[cfg(any(test, feature = "test-utils"))]
const STALE_TEST_HOME_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Removes from `group` the temporary homes that earlier test processes left
/// there, [`MAX_PRUNED_TEST_HOMES`] at most: each real directory whose name
/// starts with `prefix`, last changed more than [`STALE_TEST_HOME_AGE`] ago.
/// Nothing else in `group` is touched, a link or a file of that name
/// included, and no link is ever followed. Every error is ignored: what
/// cannot be read or removed stays, and the tests go on.
#[cfg(any(test, feature = "test-utils"))]
fn prune_stale_test_homes(group: &std::path::Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(group) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut pruned = 0;
    for entry in entries.flatten() {
        if pruned == MAX_PRUNED_TEST_HOMES {
            break;
        }
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
        {
            continue;
        }
        // The entry's own metadata, as `symlink_metadata` gives it: a link is
        // never followed, so it is never a directory here. Read from the
        // directory listing, it costs no system call on Windows, where every
        // test process lists the whole group.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let stale = metadata.is_dir()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > STALE_TEST_HOME_AGE);
        if stale {
            // Removes a link inside the home, never what it names.
            let _ = std::fs::remove_dir_all(entry.path());
            pruned += 1;
        }
    }
}

/// Serializes the orca-core tests that change the process-wide `ORCA_HOME`.
/// The tests below and the config tests share it: with two locks they would
/// overwrite each other's value under a threaded `cargo test`.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    /// Holds the environment lock and puts `ORCA_HOME`, `CARGO_MANIFEST_DIR`
    /// and this thread's overrides back the way they were, even when the test
    /// panics.
    struct HomeScope {
        _lock: MutexGuard<'static, ()>,
        original_home: Option<OsString>,
        original_manifest_dir: Option<OsString>,
    }

    impl HomeScope {
        fn new() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Self {
                _lock: lock,
                original_home: std::env::var_os(ORCA_HOME_ENV),
                original_manifest_dir: std::env::var_os(CARGO_MANIFEST_DIR_ENV),
            }
        }

        fn set_env(&self, value: &str) {
            unsafe { std::env::set_var(ORCA_HOME_ENV, value) };
        }

        fn remove_env(&self) {
            unsafe { std::env::remove_var(ORCA_HOME_ENV) };
        }

        fn set_manifest_dir(&self, value: &str) {
            unsafe { std::env::set_var(CARGO_MANIFEST_DIR_ENV, value) };
        }

        fn remove_manifest_dir(&self) {
            unsafe { std::env::remove_var(CARGO_MANIFEST_DIR_ENV) };
        }
    }

    impl Drop for HomeScope {
        fn drop(&mut self) {
            install_test_orca_home(None);
            install_host_orca_home(None);
            restore(ORCA_HOME_ENV, self.original_home.take());
            restore(CARGO_MANIFEST_DIR_ENV, self.original_manifest_dir.take());
        }
    }

    fn restore(key: &str, original: Option<OsString>) {
        unsafe {
            match original {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn orca_home_falls_back_to_a_process_temp_dir_in_tests() {
        let scope = HomeScope::new();
        scope.remove_env();

        let first = orca_home().expect("a test build always resolves a home");
        let second = orca_home().expect("a test build always resolves a home");

        assert_eq!(first, second, "the fallback is one directory per process");
        assert!(
            first.starts_with(std::env::temp_dir()),
            "{} is not under the temp dir",
            first.display()
        );
        assert_ne!(
            Some(first.clone()),
            dirs::home_dir().map(|home| home.join(".orca")),
            "a test build must never fall back to the real Orca home"
        );
        assert!(first.is_dir(), "{} was not created", first.display());
        assert_eq!(
            std::env::var_os(ORCA_HOME_ENV),
            Some(first.into_os_string()),
            "child processes inherit the same home"
        );
    }

    #[test]
    fn orca_home_prefers_the_override_then_the_environment() {
        let scope = HomeScope::new();
        scope.set_env("/a");
        assert_eq!(orca_home(), Some(PathBuf::from("/a")));
        assert_eq!(current_orca_home_override(), None);

        install_test_orca_home(Some(PathBuf::from("/b")));
        assert_eq!(orca_home(), Some(PathBuf::from("/b")));
        assert_eq!(current_orca_home_override(), Some(PathBuf::from("/b")));

        install_test_orca_home(None);
        assert_eq!(
            orca_home(),
            Some(PathBuf::from("/a")),
            "clearing the override puts the environment's home back"
        );
        assert_eq!(current_orca_home_override(), None);
    }

    #[test]
    fn orca_home_treats_a_blank_environment_value_as_unset() {
        let scope = HomeScope::new();
        scope.remove_env();
        let fallback = orca_home().expect("a test build always resolves a home");

        for blank in ["", "  "] {
            scope.set_env(blank);
            assert_eq!(
                orca_home(),
                Some(fallback.clone()),
                "ORCA_HOME={blank:?} names no home"
            );
        }
    }

    #[test]
    fn orca_home_in_a_test_utils_build_keeps_the_user_default_outside_cargo() {
        let scope = HomeScope::new();
        scope.remove_env();
        scope.remove_manifest_dir();

        // Resolving the path is all this does: the user's real home is never
        // created or written, and nothing is exported to child processes.
        assert_eq!(
            test_utils_default_home(),
            dirs::home_dir().map(|home| home.join(".orca")),
            "a test-utils binary run from a terminal keeps ~/.orca"
        );
        assert_eq!(std::env::var_os(ORCA_HOME_ENV), None);
    }

    #[test]
    fn orca_home_in_a_test_utils_build_uses_the_temp_home_under_cargo() {
        let scope = HomeScope::new();
        scope.remove_env();
        scope.set_manifest_dir("/somewhere/crates/orca-core");

        let home = test_utils_default_home().expect("a test run always has a home");

        assert_eq!(home, process_temp_home());
        assert_eq!(std::env::var_os(ORCA_HOME_ENV), Some(home.into_os_string()));
    }

    #[test]
    fn orca_home_groups_the_process_temp_homes_in_one_directory() {
        let group = std::env::temp_dir().join(TEST_HOME_GROUP);

        assert_eq!(
            process_temp_home().parent(),
            Some(group.as_path()),
            "{} is not inside {}",
            process_temp_home().display(),
            group.display()
        );
    }

    /// The temporary home adds at most 17 characters to the temp dir's path:
    /// `orca-th`, a separator, `h-` and tempfile's six random characters, and
    /// a separator the temp dir may lack. On the GitHub Windows runner that
    /// keeps the asset tests' longest path near 245 characters, under
    /// `MAX_PATH` (see [`TEST_HOME_GROUP`]); a longer group or prefix broke
    /// them.
    #[test]
    fn the_process_temp_home_keeps_its_path_short() {
        let temp_dir = std::env::temp_dir();
        let added = process_temp_home()
            .as_os_str()
            .len()
            .saturating_sub(temp_dir.as_os_str().len());
        let budget = 17;
        assert!(
            added <= budget,
            "{} adds {added} characters to {}, more than {budget}",
            process_temp_home().display(),
            temp_dir.display()
        );
    }

    #[test]
    fn orca_home_prefers_the_host_override_over_the_test_override() {
        let _scope = HomeScope::new();
        install_test_orca_home(Some(PathBuf::from("/test")));
        install_host_orca_home(Some(PathBuf::from("/host")));
        assert_eq!(orca_home(), Some(PathBuf::from("/host")));
        assert_eq!(current_orca_home_override(), Some(PathBuf::from("/host")));
        assert_eq!(current_test_orca_home(), Some(PathBuf::from("/test")));

        install_host_orca_home(None);
        assert_eq!(orca_home(), Some(PathBuf::from("/test")));
        assert_eq!(current_orca_home_override(), Some(PathBuf::from("/test")));
    }

    /// Sets when `directory` was last changed to `age` ago.
    #[cfg(unix)]
    fn last_changed(directory: &std::path::Path, age: std::time::Duration) {
        std::fs::File::open(directory)
            .and_then(|opened| opened.set_modified(std::time::SystemTime::now() - age))
            .expect("set when the directory was last changed");
    }

    #[cfg(unix)]
    const HOURS: std::time::Duration = std::time::Duration::from_secs(60 * 60);

    /// Only the test homes left more than a day ago go: a younger one stays,
    /// and so does everything else, an old entry included, and whatever a
    /// link names. A link is never followed, and never taken for a home,
    /// however old it, or what it names, is.
    #[cfg(unix)]
    #[test]
    fn pruning_removes_only_the_test_homes_left_more_than_a_day_ago() {
        let group = tempfile::tempdir().expect("a scratch group");
        let entry = |name: &str| group.path().join(name);
        let stale = entry(&format!("{TEST_HOME_PREFIX}stale"));
        let recent = entry(&format!("{TEST_HOME_PREFIX}recent"));
        let fresh = entry(&format!("{TEST_HOME_PREFIX}fresh"));
        let unrelated = entry("unrelated-stale");
        let file = entry(&format!("{TEST_HOME_PREFIX}file"));
        let target = entry("linked-stale");
        let link = entry(&format!("{TEST_HOME_PREFIX}link"));
        for directory in [&stale, &recent, &fresh, &unrelated, &target] {
            std::fs::create_dir(directory).expect("a directory");
            std::fs::write(directory.join("content"), b"").expect("its content");
        }
        std::fs::write(&file, b"").expect("a file");
        std::os::unix::fs::symlink(&target, &link).expect("a link");
        for directory in [&stale, &unrelated, &target] {
            last_changed(directory, 25 * HOURS);
        }
        last_changed(&recent, 23 * HOURS);
        // The link's own time, not its target's: `touch -h` is the portable
        // way to set it.
        let touched = std::process::Command::new("touch")
            .args(["-h", "-t", "200001010000"])
            .arg(&link)
            .status()
            .expect("run touch");
        assert!(touched.success(), "touch -h failed: {touched}");

        prune_stale_test_homes(group.path(), TEST_HOME_PREFIX);

        assert!(!stale.exists(), "the stale test home is still there");
        for kept in [&recent, &fresh, &unrelated, &target] {
            assert!(
                kept.join("content").exists(),
                "{} lost its content",
                kept.display()
            );
        }
        assert!(file.exists(), "the file went");
        assert!(
            std::fs::symlink_metadata(&link).is_ok_and(|meta| meta.file_type().is_symlink()),
            "the link went"
        );
    }

    /// After a day without tests the group can hold thousands of stale
    /// homes. One process removes a bounded number of them, so its first
    /// test is not held up for seconds; the processes after it take the rest.
    #[cfg(unix)]
    #[test]
    fn pruning_removes_a_bounded_number_of_homes_at_a_time() {
        let group = tempfile::tempdir().expect("a scratch group");
        let count = MAX_PRUNED_TEST_HOMES + 8;
        for index in 0..count {
            let home = group.path().join(format!("{TEST_HOME_PREFIX}{index}"));
            std::fs::create_dir(&home).expect("a stale test home");
            last_changed(&home, 25 * HOURS);
        }
        let left = || std::fs::read_dir(group.path()).expect("the group").count();

        prune_stale_test_homes(group.path(), TEST_HOME_PREFIX);
        assert_eq!(left(), count - MAX_PRUNED_TEST_HOMES);
        prune_stale_test_homes(group.path(), TEST_HOME_PREFIX);
        assert_eq!(left(), 0);
    }

    /// The group the temporary homes went to before e51e7a52 is pruned too,
    /// and goes once it is empty: no process makes homes there any more.
    #[cfg(unix)]
    #[test]
    fn the_first_home_of_a_process_prunes_the_legacy_group() {
        const CHILD_ENV: &str = "ORCA_TEST_HOME_PRUNING_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let _ = process_temp_home();
            return;
        }
        let temp = tempfile::tempdir().expect("a scratch temp dir");
        let legacy = temp.path().join(LEGACY_TEST_HOME_GROUP);
        let stale = legacy.join(format!("{LEGACY_TEST_HOME_PREFIX}stale"));
        std::fs::create_dir_all(&stale).expect("a stale legacy test home");
        std::fs::write(stale.join("content"), b"").expect("its content");
        last_changed(&stale, 25 * HOURS);

        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "home::tests::the_first_home_of_a_process_prunes_the_legacy_group",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .env("TMPDIR", temp.path())
            .output()
            .expect("run the test in a process of its own");

        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "the child failed ({}): {}{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(!legacy.exists(), "the legacy group is still there");
    }

    /// The first home of a process prunes the group it is created in. The
    /// test runs in a process of its own whose temp dir is a scratch one, so
    /// that it never touches the real group.
    #[cfg(unix)]
    #[test]
    fn the_first_home_of_a_process_prunes_the_stale_homes_beside_it() {
        const CHILD_ENV: &str = "ORCA_TEST_HOME_PRUNING_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let _ = process_temp_home();
            return;
        }
        let temp = tempfile::tempdir().expect("a scratch temp dir");
        let group = temp.path().join(TEST_HOME_GROUP);
        let stale = group.join(format!("{TEST_HOME_PREFIX}stale"));
        std::fs::create_dir_all(&stale).expect("a stale test home");
        last_changed(&stale, 25 * HOURS);

        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "home::tests::the_first_home_of_a_process_prunes_the_stale_homes_beside_it",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .env("TMPDIR", temp.path())
            .output()
            .expect("run the test in a process of its own");

        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "the child failed ({}): {}{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(!stale.exists(), "the stale test home is still there");
        let homes = std::fs::read_dir(&group)
            .expect("the group")
            .map(|entry| entry.expect("an entry").file_name())
            .collect::<Vec<_>>();
        assert!(
            homes.len() == 1
                && homes[0]
                    .to_str()
                    .is_some_and(|name| name.starts_with(TEST_HOME_PREFIX)),
            "the group holds {homes:?}, not just the child's own home"
        );
    }
}
