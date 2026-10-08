//! Durable first-run disclosure acknowledgement.
//!
//! This module records only that a workspace/security-policy disclosure was
//! shown. It never stores credentials or changes folder trust.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orca_core::config::file::{self, AUTH_FILE};
use orca_core::config::folder_trust;
use orca_core::config::{DelegationSnapshot, RunConfig};
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::diagnostics::{self, DiagnosticReport, DoctorOptions};

pub const ONBOARDING_SCHEMA_VERSION: u32 = 1;
const ACKNOWLEDGEMENT_FILE: &str = "onboarding.toml";
const ACKNOWLEDGEMENT_LOCK: &str = "onboarding.lock";
const MAX_ACKNOWLEDGEMENT_BYTES: u64 = 1024 * 1024;
const MAX_ACKNOWLEDGEMENTS: usize = 1024;

#[derive(Clone, Debug)]
pub struct FirstRunState {
    pub schema_version: u32,
    pub workspace: PathBuf,
    pub config_dir: PathBuf,
    pub auth_path: PathBuf,
    pub acknowledgement_path: PathBuf,
    pub security_policy_digest: String,
    pub acknowledged: bool,
    pub workspace_trusted: bool,
    pub diagnostics: DiagnosticReport,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AcknowledgementStore {
    schema_version: u32,
    #[serde(default)]
    acknowledgements: Vec<Acknowledgement>,
}

impl Default for AcknowledgementStore {
    fn default() -> Self {
        Self {
            schema_version: ONBOARDING_SCHEMA_VERSION,
            acknowledgements: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Acknowledgement {
    workspace: PathBuf,
    security_policy_digest: String,
    acknowledged_at: DateTime<Utc>,
}

pub fn inspect_first_run(config: &RunConfig) -> io::Result<FirstRunState> {
    let config_dir = file::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot resolve ORCA_HOME for first-run acknowledgement",
        )
    })?;
    inspect_first_run_in(config, &config_dir)
}

pub fn inspect_first_run_in(config: &RunConfig, config_dir: &Path) -> io::Result<FirstRunState> {
    let workspace = config
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let config_dir = canonicalize_allow_missing(config_dir)?;
    let acknowledgement_path = config_dir.join(ACKNOWLEDGEMENT_FILE);
    let security_policy_digest = security_policy_digest(config, &workspace)?;
    let store = read_store(&acknowledgement_path);
    let acknowledged = store.acknowledgements.iter().any(|entry| {
        entry.workspace == workspace && entry.security_policy_digest == security_policy_digest
    });

    Ok(FirstRunState {
        schema_version: ONBOARDING_SCHEMA_VERSION,
        workspace: workspace.clone(),
        auth_path: config_dir.join(AUTH_FILE),
        acknowledgement_path,
        security_policy_digest,
        acknowledged,
        workspace_trusted: folder_trust::is_trusted_with_config_dir(&workspace, &config_dir),
        diagnostics: diagnostics::collect_doctor(DoctorOptions {
            cwd: Some(workspace),
        }),
        config_dir,
    })
}

pub fn acknowledge_first_run(state: &FirstRunState) -> io::Result<()> {
    acknowledge_first_run_in(state)
}

pub fn acknowledge_first_run_in(state: &FirstRunState) -> io::Result<()> {
    if state.schema_version != ONBOARDING_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "first-run acknowledgement schema is stale",
        ));
    }
    let expected_path = state.config_dir.join(ACKNOWLEDGEMENT_FILE);
    if expected_path != state.acknowledgement_path {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "first-run acknowledgement path escaped ORCA_HOME",
        ));
    }

    fs::create_dir_all(&state.config_dir)?;
    let _lock = ExclusiveFileLock::acquire(&state.config_dir.join(ACKNOWLEDGEMENT_LOCK))
        .map_err(io::Error::other)?;
    let mut store = read_store(&state.acknowledgement_path);
    if !store.acknowledgements.iter().any(|entry| {
        entry.workspace == state.workspace
            && entry.security_policy_digest == state.security_policy_digest
    }) {
        if store.acknowledgements.len() >= MAX_ACKNOWLEDGEMENTS {
            let remove = store
                .acknowledgements
                .len()
                .saturating_sub(MAX_ACKNOWLEDGEMENTS - 1);
            store.acknowledgements.drain(0..remove);
        }
        store.acknowledgements.push(Acknowledgement {
            workspace: state.workspace.clone(),
            security_policy_digest: state.security_policy_digest.clone(),
            acknowledged_at: Utc::now(),
        });
    }
    let encoded = toml::to_string_pretty(&store).map_err(io::Error::other)?;
    atomic_write(
        &state.acknowledgement_path,
        encoded.as_bytes(),
        AtomicWritePolicy::NoFollow,
    )
    .map_err(io::Error::other)
}

fn read_store(path: &Path) -> AcknowledgementStore {
    let Ok(metadata) = path.metadata() else {
        return AcknowledgementStore::default();
    };
    if !metadata.is_file() || metadata.len() > MAX_ACKNOWLEDGEMENT_BYTES {
        return AcknowledgementStore::default();
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return AcknowledgementStore::default();
    };
    let Ok(store) = toml::from_str::<AcknowledgementStore>(&contents) else {
        return AcknowledgementStore::default();
    };
    if store.schema_version != ONBOARDING_SCHEMA_VERSION
        || store.acknowledgements.len() > MAX_ACKNOWLEDGEMENTS
    {
        return AcknowledgementStore::default();
    }
    store
}

fn security_policy_digest(config: &RunConfig, workspace: &Path) -> io::Result<String> {
    let policy = serde_json::json!({
        "schema_version": ONBOARDING_SCHEMA_VERSION,
        "workspace": workspace,
        "delegation": DelegationSnapshot::from_config(config),
        "tools": &config.tools,
        "workflow_capabilities": &config.workflows.capabilities,
    });
    let canonical = canonical_json(&policy);
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).expect("string serializes"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serializes"),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn canonicalize_allow_missing(path: &Path) -> io::Result<PathBuf> {
    if path.exists() {
        return path.canonicalize();
    }

    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let name = cursor.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ORCA_HOME has no existing ancestor",
            )
        })?;
        missing.push(name.to_os_string());
        cursor = cursor.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ORCA_HOME has no existing ancestor",
            )
        })?;
    }
    let mut canonical = cursor.canonicalize()?;
    for name in missing.into_iter().rev() {
        canonical.push(name);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticCwd;

    /// The state `inspect_first_run_in` returns for `workspace`, as far as
    /// `acknowledge_first_run_in` reads it. The workspace need not exist.
    fn first_run_state(
        config_dir: &Path,
        workspace: &str,
        security_policy_digest: &str,
    ) -> FirstRunState {
        FirstRunState {
            schema_version: ONBOARDING_SCHEMA_VERSION,
            workspace: PathBuf::from(workspace),
            config_dir: config_dir.to_path_buf(),
            auth_path: config_dir.join(AUTH_FILE),
            acknowledgement_path: config_dir.join(ACKNOWLEDGEMENT_FILE),
            security_policy_digest: security_policy_digest.to_string(),
            acknowledged: false,
            workspace_trusted: false,
            diagnostics: DiagnosticReport {
                schema_version: 1,
                package: "orca",
                website: "",
                version: String::new(),
                platform: String::new(),
                cwd: DiagnosticCwd {
                    requested: workspace.to_string(),
                    canonical: None,
                },
                checks: Vec::new(),
            },
        }
    }

    /// The acknowledgement file v0.5.7 wrote with `acknowledge_first_run_in`,
    /// as the TOML library of that release laid it out: workspaces with
    /// spaces and Chinese characters in their names, and a Windows path with
    /// backslashes (written as a literal string). A user's file stays in
    /// this form until the next acknowledgement rewrites it, so a newer
    /// library has to read it as it is.
    const ACKNOWLEDGEMENT_FILE_V0_5_7: &str = r#"schema_version = 1

[[acknowledgements]]
workspace = "/Users/dev/projects/my app"
security_policy_digest = "aa1e03b9d98286428b5fd1e3ccf3b999a20fa8dedf7ed8b7310f3e211417e51a"
acknowledged_at = "2026-10-08T04:21:38.905745Z"

[[acknowledgements]]
workspace = "/Users/dev/项目/测试 目录"
security_policy_digest = "cbe0df110917259d1814dffef075983390aa87f1745004833c54c789054a60c7"
acknowledged_at = "2026-10-08T04:21:38.920209Z"

[[acknowledgements]]
workspace = '\\?\C:\Users\dev\工作 区'
security_policy_digest = "bc99adfdecebc9bbbcce3dd81dd97feaf8afcddbe92b79b7c96c2bb7fd090fbc"
acknowledged_at = "2026-10-08T04:21:38.934146Z"
"#;

    /// Every acknowledgement of `ACKNOWLEDGEMENT_FILE_V0_5_7`: the workspace,
    /// the security-policy digest, and the time.
    const ACKNOWLEDGEMENTS_V0_5_7: [(&str, &str, &str); 3] = [
        (
            "/Users/dev/projects/my app",
            "aa1e03b9d98286428b5fd1e3ccf3b999a20fa8dedf7ed8b7310f3e211417e51a",
            "2026-10-08T04:21:38.905745Z",
        ),
        (
            "/Users/dev/项目/测试 目录",
            "cbe0df110917259d1814dffef075983390aa87f1745004833c54c789054a60c7",
            "2026-10-08T04:21:38.920209Z",
        ),
        (
            r"\\?\C:\Users\dev\工作 区",
            "bc99adfdecebc9bbbcce3dd81dd97feaf8afcddbe92b79b7c96c2bb7fd090fbc",
            "2026-10-08T04:21:38.934146Z",
        ),
    ];

    #[test]
    fn an_acknowledgement_file_in_the_v0_5_7_format_still_loads() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(ACKNOWLEDGEMENT_FILE);
        fs::write(&path, ACKNOWLEDGEMENT_FILE_V0_5_7).unwrap();

        let store = read_store(&path);

        // A file that does not load is an empty store, which has
        // acknowledged nothing: the count says that apart from a file that
        // does.
        assert_eq!(store.schema_version, ONBOARDING_SCHEMA_VERSION);
        assert_eq!(
            store.acknowledgements.len(),
            ACKNOWLEDGEMENTS_V0_5_7.len(),
            "{store:?}"
        );
        for (entry, (workspace, digest, at)) in
            store.acknowledgements.iter().zip(ACKNOWLEDGEMENTS_V0_5_7)
        {
            assert_eq!(entry.workspace, Path::new(workspace));
            assert_eq!(entry.security_policy_digest, digest);
            assert_eq!(entry.acknowledged_at, at.parse::<DateTime<Utc>>().unwrap());
        }
    }

    #[test]
    fn a_saved_acknowledgement_file_reads_back_with_toml_0_8() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(ACKNOWLEDGEMENT_FILE);
        // The workspaces of v0.5.7's file, and one whose name needs escapes:
        // a tab, a line break, the escape character, an emoji, both kinds of
        // quote and a trailing backslash.
        let mut saved: Vec<(String, String)> = ACKNOWLEDGEMENTS_V0_5_7
            .iter()
            .map(|(workspace, digest, _)| (workspace.to_string(), digest.to_string()))
            .collect();
        saved.push((
            "/srv/tab\there/new\nline/escape\u{1b}[0m \u{1f980} 'single' \"double\"\\".to_string(),
            "ee".repeat(32),
        ));
        for (workspace, digest) in &saved {
            acknowledge_first_run_in(&first_run_state(home.path(), workspace, digest)).unwrap();
        }

        // A release that still has toml 0.8 reads the file this one writes:
        // as TOML, and as the store it loads, which holds nothing when the
        // file does not parse.
        let text = fs::read_to_string(&path).unwrap();
        toml_v08::from_str::<toml_v08::Table>(&text)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let by_toml_0_8: AcknowledgementStore = toml_v08::from_str(&text).unwrap();
        let by_this_release = read_store(&path);

        let read_back = |store: &AcknowledgementStore| -> Vec<(String, String)> {
            store
                .acknowledgements
                .iter()
                .map(|entry| {
                    (
                        entry.workspace.to_str().unwrap().to_string(),
                        entry.security_policy_digest.clone(),
                    )
                })
                .collect()
        };
        for store in [&by_toml_0_8, &by_this_release] {
            assert_eq!(store.schema_version, ONBOARDING_SCHEMA_VERSION);
            assert_eq!(read_back(store), saved);
        }
        let times = |store: &AcknowledgementStore| -> Vec<DateTime<Utc>> {
            store
                .acknowledgements
                .iter()
                .map(|entry| entry.acknowledged_at)
                .collect()
        };
        assert_eq!(times(&by_toml_0_8), times(&by_this_release));
    }
}
