//! Folder trust: a persisted allowlist of directories the user has approved
//! for write/network sandbox modes.
//!
//! Untrusted directories use a strict, fail-closed read-only default. Explicit
//! permission profiles and sandbox policies remain authoritative, matching the
//! existing approval and capability model.
//!
//! The store is a small TOML file under the config dir (`ORCA_HOME` or
//! `~/.orca`). Trust decisions are keyed by canonicalized absolute path.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};

const ORCA_HOME_ENV: &str = "ORCA_HOME";
const TRUST_FILE: &str = "folder_trust.toml";

// Test-support override machinery. The orca-runtime test harness installs
// per-thread `ORCA_HOME` overrides here instead of mutating the environment
// (the process-wide `ORCA_HOME` stays on the isolated test home), so a test
// and the hosts it spawns resolve a private home while every other test
// keeps resolving the process-wide one. Production code never installs
// these; resolution mirrors `orca_runtime::history::read_test_orca_home`
// (host override, then test override, then the environment).
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
/// override), as consulted by `config_dir` and the orca-runtime test harness.
#[doc(hidden)]
pub fn current_orca_home_override() -> Option<PathBuf> {
    test_home_override()
}

fn test_home_override() -> Option<PathBuf> {
    HOST_ORCA_HOME
        .with(|cell| cell.borrow().clone())
        .or_else(|| TEST_ORCA_HOME.with(|cell| cell.borrow().clone()))
}

/// Trust state for a single directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Full sandbox modes (workspace write, network) are allowed to apply.
    Trusted,
    /// The directory was explicitly marked untrusted; always fail closed.
    Untrusted,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    folders: BTreeMap<String, TrustEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TrustEntry {
    level: TrustLevel,
}

/// Resolve the user-owned Orca configuration directory. Project-local files
/// must never influence this location. Under test, a per-thread override
/// installed by the orca-runtime harness wins over the environment.
pub fn config_dir() -> Option<PathBuf> {
    test_home_override()
        .or_else(|| std::env::var_os(ORCA_HOME_ENV).map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".orca")))
}

fn trust_file_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(TRUST_FILE))
}

fn trust_file_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(TRUST_FILE)
}

/// Canonicalize a path for use as a stable trust key. Falls back to a
/// lexical absolute form when the path does not yet exist on disk.
fn trust_key(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| absolutize(path));
    canonical.to_string_lossy().into_owned()
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn load_path(path: &Path) -> TrustFile {
    let Ok(content) = fs::read_to_string(path) else {
        return TrustFile::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

fn load() -> TrustFile {
    trust_file_path()
        .as_deref()
        .map(load_path)
        .unwrap_or_default()
}

fn save_path(path: &Path, store: &TrustFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let serialized =
        toml::to_string_pretty(store).map_err(|e| format!("serializing folder trust: {e}"))?;
    atomic_write(path, serialized.as_bytes(), AtomicWritePolicy::NoFollow)
        .map_err(|error| format!("replacing {}: {error}", path.display()))
}

fn save(store: &TrustFile) -> Result<(), String> {
    let Some(path) = trust_file_path() else {
        return Err("no config directory available for folder trust".to_string());
    };
    save_path(&path, store)
}

/// Look up the trust level for `path`, treating any ancestor trust decision as
/// applying to descendants. Returns `None` when no decision has been recorded.
pub fn trust_level(path: &Path) -> Option<TrustLevel> {
    let store = load();
    trust_level_in(&store, path)
}

/// Resolve trust using an explicit user configuration directory. This keeps
/// project-local configuration from influencing where trust decisions live.
pub fn trust_level_with_config_dir(path: &Path, config_dir: &Path) -> Option<TrustLevel> {
    let store = load_path(&trust_file_path_in(config_dir));
    trust_level_in(&store, path)
}

fn trust_level_in(store: &TrustFile, path: &Path) -> Option<TrustLevel> {
    let target = PathBuf::from(trust_key(path));
    // Exact match wins; otherwise the nearest recorded ancestor applies. An
    // explicit `Untrusted` on a closer ancestor overrides a `Trusted` further
    // up the tree.
    let mut best: Option<(usize, TrustLevel)> = None;
    for (key, entry) in &store.folders {
        let key_path = PathBuf::from(key);
        if target == key_path || target.starts_with(&key_path) {
            let depth = key_path.components().count();
            if best.map(|(d, _)| depth > d).unwrap_or(true) {
                best = Some((depth, entry.level));
            }
        }
    }
    best.map(|(_, level)| level)
}

/// Returns true when `path` may use full (write/network) sandbox modes.
/// Directories with no recorded decision are treated as untrusted so that new
/// workspaces fail closed by default.
pub fn is_trusted(path: &Path) -> bool {
    matches!(trust_level(path), Some(TrustLevel::Trusted))
}

pub fn is_trusted_with_config_dir(path: &Path, config_dir: &Path) -> bool {
    matches!(
        trust_level_with_config_dir(path, config_dir),
        Some(TrustLevel::Trusted)
    )
}

/// Record a trust decision for `path`.
pub fn set_trust(path: &Path, level: TrustLevel) -> Result<(), String> {
    let mut store = load();
    store.folders.insert(trust_key(path), TrustEntry { level });
    save(&store)
}

pub fn set_trust_with_config_dir(
    path: &Path,
    config_dir: &Path,
    level: TrustLevel,
) -> Result<(), String> {
    let trust_path = trust_file_path_in(config_dir);
    let mut store = load_path(&trust_path);
    store.folders.insert(trust_key(path), TrustEntry { level });
    save_path(&trust_path, &store)
}

/// Remove any recorded trust decision for `path` (exact key only).
pub fn clear_trust(path: &Path) -> Result<(), String> {
    let mut store = load();
    if store.folders.remove(&trust_key(path)).is_some() {
        save(&store)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_folder_is_untrusted_by_default() {
        let home = tempfile::tempdir().unwrap();
        assert!(!is_trusted_with_config_dir(
            Path::new("/some/unknown/project"),
            home.path()
        ));
        assert_eq!(
            trust_level_with_config_dir(Path::new("/some/unknown/project"), home.path()),
            None
        );
    }

    #[test]
    fn trusted_folder_roundtrips_and_covers_descendants() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("project");
        fs::create_dir_all(&root).unwrap();
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();

        assert!(is_trusted_with_config_dir(&root, home.path()));
        // Descendants inherit trust.
        let child = root.join("nested");
        fs::create_dir_all(&child).unwrap();
        assert!(is_trusted_with_config_dir(&child, home.path()));
    }

    #[test]
    fn explicit_untrusted_child_overrides_trusted_parent() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let child = root.join("secret");
        fs::create_dir_all(&child).unwrap();
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();
        set_trust_with_config_dir(&child, home.path(), TrustLevel::Untrusted).unwrap();

        assert!(is_trusted_with_config_dir(&root, home.path()));
        assert_eq!(
            trust_level_with_config_dir(&child, home.path()),
            Some(TrustLevel::Untrusted)
        );
        assert!(!is_trusted_with_config_dir(&child, home.path()));
    }

    #[test]
    fn malformed_trust_store_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(TRUST_FILE), "not valid [toml").unwrap();

        assert!(!is_trusted_with_config_dir(home.path(), home.path()));
    }
}
