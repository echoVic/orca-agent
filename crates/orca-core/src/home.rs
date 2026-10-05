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
//! is private to the process. A test build is this crate's own `cfg(test)` or
//! any build that enables the `test-utils` feature; other crates enable it
//! through a dev-dependency, so release builds keep the `~/.orca` default.

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

#[cfg(not(any(test, feature = "test-utils")))]
fn default_orca_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".orca"))
}

/// Falls back to the process's temporary home and exports it as `ORCA_HOME`,
/// so child processes land in the same directory rather than creating their
/// own.
#[cfg(any(test, feature = "test-utils"))]
fn default_orca_home() -> Option<PathBuf> {
    let home = process_temp_home();
    // edition 2024: set_var is unsafe. Only reached while `ORCA_HOME` names
    // no home, and always with the same value.
    unsafe {
        std::env::set_var(ORCA_HOME_ENV, home);
    }
    Some(home.to_path_buf())
}

/// The temporary home every test build in this process falls back to. It is
/// created on first use and this process never removes it: a detached child
/// may still be writing to it when this one exits. This function does not
/// touch the environment.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn process_temp_home() -> &'static std::path::Path {
    use std::sync::OnceLock;

    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("orca-test-home-")
            .tempdir()
            .expect("create the process-wide test Orca home")
    })
    .path()
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

    /// Holds the environment lock and puts `ORCA_HOME` and this thread's
    /// overrides back the way they were, even when the test panics.
    struct HomeScope {
        _lock: MutexGuard<'static, ()>,
        original: Option<OsString>,
    }

    impl HomeScope {
        fn new() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Self {
                _lock: lock,
                original: std::env::var_os(ORCA_HOME_ENV),
            }
        }

        fn set_env(&self, value: &str) {
            unsafe { std::env::set_var(ORCA_HOME_ENV, value) };
        }

        fn remove_env(&self) {
            unsafe { std::env::remove_var(ORCA_HOME_ENV) };
        }
    }

    impl Drop for HomeScope {
        fn drop(&mut self) {
            install_test_orca_home(None);
            install_host_orca_home(None);
            unsafe {
                match self.original.take() {
                    Some(value) => std::env::set_var(ORCA_HOME_ENV, value),
                    None => std::env::remove_var(ORCA_HOME_ENV),
                }
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
}
