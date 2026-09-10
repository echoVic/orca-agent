use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

const ORCA_HOME_ENV: &str = "ORCA_HOME";
const CACHE_SUBDIR: &str = "summary_cache";

/// Content-addressed key for a summary request. Combining the previous summary
/// with the collapsed delta makes the key stable across resume/fork/replay:
/// summarizing the same older history (after deterministic extractive
/// compaction) always hashes to the same value, so the remote summary call is
/// skipped entirely on repeated compaction of identical content. The `purpose`
/// segment keeps semantically different requests (incremental delta summary vs
/// baseline rebuild) in separate cache spaces even when their text collides.
pub fn summary_key(
    scope: &str,
    purpose: &str,
    previous_summary: Option<&str>,
    delta_text: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"orca-summary-v3\n");
    hasher.update(b"scope:");
    hasher.update(scope.as_bytes());
    hasher.update(b"\npurpose:");
    hasher.update(purpose.as_bytes());
    match previous_summary {
        Some(prev) => {
            hasher.update(b"\nprev:");
            hasher.update(prev.as_bytes());
        }
        None => hasher.update(b"\nprev:<none>"),
    }
    hasher.update(b"\ndelta:");
    hasher.update(delta_text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn lookup(key: &str) -> Option<String> {
    let path = cache_path(key)?;
    let cached = fs::read_to_string(path).ok()?;
    let trimmed = cached.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn store(key: &str, summary: &str) {
    let Some(path) = cache_path(key) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = fs::write(path, summary);
}

fn cache_path(key: &str) -> Option<PathBuf> {
    if key.is_empty() {
        return None;
    }
    Some(cache_dir()?.join(format!("{key}.txt")))
}

fn cache_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        thread_local! {
            static ROOT: tempfile::TempDir = tempfile::tempdir().expect("isolated summary cache");
        }
        Some(ROOT.with(|root| root.path().join(CACHE_SUBDIR)))
    }
    #[cfg(not(test))]
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join(CACHE_SUBDIR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os(ORCA_HOME_ENV);
            unsafe {
                std::env::set_var(ORCA_HOME_ENV, path);
            }
            Self { previous }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(ORCA_HOME_ENV, value),
                    None => std::env::remove_var(ORCA_HOME_ENV),
                }
            }
        }
    }

    #[test]
    fn key_is_stable_for_identical_inputs() {
        let a = summary_key(
            "provider=deepseek;model=aux;prompt=v1",
            "delta",
            Some("baseline"),
            "delta body",
        );
        let b = summary_key(
            "provider=deepseek;model=aux;prompt=v1",
            "delta",
            Some("baseline"),
            "delta body",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn key_changes_with_previous_summary_and_delta() {
        let scope = "provider=deepseek;model=aux;prompt=v1";
        let base = summary_key(scope, "delta", None, "delta");
        assert_ne!(base, summary_key(scope, "delta", Some("prev"), "delta"));
        assert_ne!(base, summary_key(scope, "delta", None, "different delta"));
    }

    #[test]
    fn key_changes_with_purpose() {
        let scope = "provider=deepseek;model=aux;prompt=v1";
        let delta = summary_key(scope, "delta", None, "same text");
        let rebuild = summary_key(scope, "rebuild_baseline", None, "same text");
        assert_ne!(
            delta, rebuild,
            "different purposes must not collide in cache space"
        );
    }

    #[test]
    fn key_changes_with_summary_scope() {
        let base = summary_key(
            "provider=deepseek;model=aux;prompt=v1",
            "delta",
            None,
            "delta",
        );
        assert_ne!(
            base,
            summary_key("provider=mock;model=aux;prompt=v1", "delta", None, "delta")
        );
        assert_ne!(
            base,
            summary_key(
                "provider=deepseek;model=other-aux;prompt=v1",
                "delta",
                None,
                "delta"
            )
        );
        assert_ne!(
            base,
            summary_key(
                "provider=deepseek;model=aux;prompt=v2",
                "delta",
                None,
                "delta"
            )
        );
    }

    #[test]
    fn store_then_lookup_round_trips() {
        let _env_guard = ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let _guard = HomeGuard::set(home.path());

        let key = summary_key(
            "provider=deepseek;model=aux;prompt=v1",
            "delta",
            None,
            "collapsed content",
        );
        assert!(lookup(&key).is_none());
        store(&key, "the summary text");
        assert_eq!(lookup(&key).as_deref(), Some("the summary text"));
    }

    #[test]
    fn lookup_miss_for_unknown_key() {
        let _env_guard = ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let _guard = HomeGuard::set(home.path());

        assert!(
            lookup(&summary_key(
                "provider=deepseek;model=aux;prompt=v1",
                "delta",
                None,
                "never stored"
            ))
            .is_none()
        );
    }
}
