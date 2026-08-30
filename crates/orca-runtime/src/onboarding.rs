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
