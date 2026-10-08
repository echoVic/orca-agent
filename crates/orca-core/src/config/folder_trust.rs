//! Folder trust: a persisted allowlist of directories the user has approved
//! for write/network sandbox modes.
//!
//! Untrusted directories use a strict, fail-closed read-only default. Explicit
//! permission profiles and sandbox policies remain authoritative, matching the
//! existing approval and capability model.
//!
//! The store is a small TOML file under the config dir (`ORCA_HOME` or
//! `~/.orca`). Trust decisions are keyed by canonicalized absolute path.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};

const TRUST_FILE: &str = "folder_trust.toml";

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
/// must never influence this location.
pub fn config_dir() -> Option<PathBuf> {
    crate::home::orca_home()
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
    canonicalize_as_far_as_possible(path)
        .to_string_lossy()
        .into_owned()
}

/// Canonical form that does not depend on whether `path` itself exists: canonicalize the
/// nearest existing ancestor (resolving symlinks and platform mappings such as macOS'
/// `/tmp` → `/private/tmp`) and append the remaining components lexically.
///
/// The previous form fell back to a purely lexical path when the directory was missing, so a
/// decision recorded before `mkdir` never matched the lookup made afterwards (issue #75).
fn canonicalize_as_far_as_possible(path: &Path) -> PathBuf {
    let absolute = absolutize(path);
    let mut candidate = absolute.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = candidate.canonicalize() {
            let mut resolved = canonical;
            for component in tail.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        match (candidate.parent(), candidate.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                candidate = parent;
            }
            _ => return absolute,
        }
    }
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
    // Both spellings are consulted: `trust_key` (canonical-as-far-as-possible) and the purely
    // lexical form, so entries recorded by older versions — which used the lexical key for
    // paths that did not exist yet — keep applying (issue #75).
    let targets = [PathBuf::from(trust_key(path)), absolutize(path)];
    // Exact match wins; otherwise the nearest recorded ancestor applies. An
    // explicit `Untrusted` on a closer ancestor overrides a `Trusted` further
    // up the tree.
    let mut best: Option<(usize, TrustLevel)> = None;
    for (key, entry) in &store.folders {
        let key_path = PathBuf::from(key);
        if targets
            .iter()
            .any(|target| target == &key_path || target.starts_with(&key_path))
        {
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
    let Some(config_dir) = config_dir() else {
        return Err("no config directory available for folder trust".to_string());
    };
    set_trust_with_config_dir(path, &config_dir, level)
}

pub fn set_trust_with_config_dir(
    path: &Path,
    config_dir: &Path,
    level: TrustLevel,
) -> Result<(), String> {
    with_trust_lock(config_dir, || {
        let trust_path = trust_file_path_in(config_dir);
        let mut store = load_path(&trust_path);
        store.folders.insert(trust_key(path), TrustEntry { level });
        save_path(&trust_path, &store)
    })
}

/// Serialize the read-modify-write of the trust store across processes.
///
/// The store is one TOML file rewritten as a whole, so two concurrent writers that both read
/// before either writes lose one decision — every command still exits 0 (issue #81; sixteen
/// concurrent `trust add` calls kept two entries). The lock is held across load→modify→save;
/// readers stay lock-free because `atomic_write` only ever publishes a whole file.
fn with_trust_lock<T>(
    config_dir: &Path,
    update: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut lock_path = trust_file_path_in(config_dir).into_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let _lock = orca_platform::fs::ExclusiveFileLock::acquire(&lock_path)
        .map_err(|error| format!("locking {}: {error}", lock_path.display()))?;
    update()
}

/// Remove any recorded trust decision for `path` (exact key only).
pub fn clear_trust(path: &Path) -> Result<(), String> {
    let Some(config_dir) = config_dir() else {
        return Err("no config directory available for folder trust".to_string());
    };
    with_trust_lock(&config_dir, || {
        let trust_path = trust_file_path_in(&config_dir);
        let mut store = load_path(&trust_path);
        if store.folders.remove(&trust_key(path)).is_some() {
            save_path(&trust_path, &store)?;
        }
        Ok(())
    })
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
    fn trust_survives_the_directory_appearing() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("not-created-yet/deeper");
        set_trust_with_config_dir(&project, home.path(), TrustLevel::Trusted).unwrap();
        assert!(
            is_trusted_with_config_dir(&project, home.path()),
            "before mkdir"
        );

        fs::create_dir_all(&project).unwrap();
        assert!(
            is_trusted_with_config_dir(&project, home.path()),
            "the decision must still apply once the directory exists"
        );
        assert!(is_trusted_with_config_dir(
            &project.join("child"),
            home.path()
        ));
    }

    #[test]
    fn lexical_entries_from_older_stores_still_apply() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("legacy");
        // The shape an older version wrote for a path that did not exist yet.
        let legacy_key = absolutize(&project).to_string_lossy().into_owned();
        let mut store = TrustFile::default();
        store.folders.insert(
            legacy_key,
            TrustEntry {
                level: TrustLevel::Trusted,
            },
        );
        save_path(&trust_file_path_in(home.path()), &store).unwrap();

        fs::create_dir_all(&project).unwrap();
        assert!(is_trusted_with_config_dir(&project, home.path()));
        assert!(is_trusted_with_config_dir(
            &project.join("nested"),
            home.path()
        ));
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

    /// The trust file v0.5.7 wrote with `save_path`, as the TOML library of
    /// that release laid it out: folders with spaces and Chinese characters
    /// in their names, Windows paths with backslashes (a key written as a
    /// literal string), and a path with both kinds of quote in it (a key
    /// written with escapes). A user's file stays in this form until the next
    /// decision rewrites it, so a newer library has to read it as it is.
    const TRUST_FILE_V0_5_7: &str = r#"[folders."/Users/dev/projects/my app"]
level = "trusted"

[folders."/Users/dev/项目"]
level = "trusted"

[folders."/Users/dev/项目/测试 目录"]
level = "untrusted"

[folders."/srv/it's \"quoted\""]
level = "trusted"

[folders.'C:\Users\dev\My Project']
level = "trusted"

[folders.'\\?\C:\Users\dev\工作 区']
level = "untrusted"
"#;

    /// Every decision of `TRUST_FILE_V0_5_7`.
    const TRUST_DECISIONS_V0_5_7: [(&str, TrustLevel); 6] = [
        ("/Users/dev/projects/my app", TrustLevel::Trusted),
        ("/Users/dev/项目", TrustLevel::Trusted),
        ("/Users/dev/项目/测试 目录", TrustLevel::Untrusted),
        (r#"/srv/it's "quoted""#, TrustLevel::Trusted),
        (r"C:\Users\dev\My Project", TrustLevel::Trusted),
        (r"\\?\C:\Users\dev\工作 区", TrustLevel::Untrusted),
    ];

    #[test]
    fn a_trust_file_in_the_v0_5_7_format_still_loads() {
        let home = tempfile::tempdir().unwrap();
        let path = trust_file_path_in(home.path());
        fs::write(&path, TRUST_FILE_V0_5_7).unwrap();

        let store = load_path(&path);

        // A file that does not parse loads as an empty store, which trusts
        // nothing: the count says that apart from a file that parses.
        assert_eq!(
            store.folders.len(),
            TRUST_DECISIONS_V0_5_7.len(),
            "{:?}",
            store.folders
        );
        for (folder, level) in TRUST_DECISIONS_V0_5_7 {
            assert_eq!(
                store.folders.get(folder).map(|entry| entry.level),
                Some(level),
                "{folder}"
            );
        }
    }

    /// The decisions of `TRUST_FILE_V0_5_7` and of folders whose names need
    /// an escape: a tab, a line break, the escape and delete characters, a
    /// trailing backslash, and both kinds of quote beside an emoji.
    fn decisions_to_save() -> Vec<(String, TrustLevel)> {
        let awkward = [
            "/srv/tab\there",
            "/srv/new\nline",
            "/srv/escape\u{1b}[0m",
            "/srv/delete\u{7f}",
            "/srv/ends-with-a-backslash\\",
            "/srv/crab \u{1f980} with 'single' and \"double\" quotes",
        ];
        TRUST_DECISIONS_V0_5_7
            .into_iter()
            .chain(
                awkward
                    .into_iter()
                    .map(|folder| (folder, TrustLevel::Untrusted)),
            )
            .map(|(folder, level)| (folder.to_string(), level))
            .collect()
    }

    fn decisions_of(store: &TrustFile) -> Vec<(String, TrustLevel)> {
        store
            .folders
            .iter()
            .map(|(folder, entry)| (folder.clone(), entry.level))
            .collect()
    }

    #[test]
    fn a_saved_trust_file_reads_back_with_toml_0_8() {
        let home = tempfile::tempdir().unwrap();
        let path = trust_file_path_in(home.path());
        let mut saved = TrustFile::default();
        for (folder, level) in decisions_to_save() {
            saved.folders.insert(folder, TrustEntry { level });
        }

        save_path(&path, &saved).unwrap();

        // A release that still has toml 0.8 reads the file this one writes:
        // as TOML, and as the store it loads, which holds nothing when the
        // file does not parse.
        let text = fs::read_to_string(&path).unwrap();
        toml_v08::from_str::<toml_v08::Table>(&text)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let by_toml_0_8: TrustFile = toml_v08::from_str(&text).unwrap();
        assert_eq!(decisions_of(&by_toml_0_8), decisions_of(&saved));
        assert_eq!(decisions_of(&load_path(&path)), decisions_of(&saved));
    }

    #[test]
    fn malformed_trust_store_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(TRUST_FILE), "not valid [toml").unwrap();

        assert!(!is_trusted_with_config_dir(home.path(), home.path()));
    }
}
