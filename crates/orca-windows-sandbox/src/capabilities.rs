use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, PathIdentity, atomic_write};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::WindowsSandboxError;

const CAPABILITY_STATE_VERSION: u32 = 1;
const SETUP_RECEIPT_VERSION: u32 = 1;
const SETUP_RECEIPT_FILE: &str = "setup-receipt.json";
const CAPABILITY_STATE_FILE: &str = "capabilities.json";
pub const SETUP_HELPER_VERSION: &str = "orca-windows-sandbox-setup-v1";

#[derive(Clone, Debug)]
pub struct CapabilityStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSetupReceipt {
    pub version: u32,
    pub helper_version: String,
    pub workspace: String,
    pub read_only_sid: String,
    pub write_sid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CapabilityState {
    version: u32,
    read_only_sid: String,
    write_root_sids: BTreeMap<String, String>,
}

impl CapabilityStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn read_only_sid(&self) -> Result<String, WindowsSandboxError> {
        self.with_state(|state| Ok((state.read_only_sid.clone(), false)))
    }

    pub fn write_sid(&self, root: &Path) -> Result<String, WindowsSandboxError> {
        let identity = PathIdentity::windows(&root.to_string_lossy())?;
        let key = identity.storage_key();
        self.with_state(|state| {
            if let Some(sid) = state.write_root_sids.get(&key) {
                return Ok((sid.clone(), false));
            }
            let sid = new_capability_sid();
            state.write_root_sids.insert(key, sid.clone());
            Ok((sid, true))
        })
    }

    /// Provision the capability state and persist the receipt consumed by the
    /// runtime before it admits a restricted Windows process.
    pub fn provision_setup(
        &self,
        workspace: &Path,
        helper_version: &str,
    ) -> Result<SandboxSetupReceipt, WindowsSandboxError> {
        let workspace = normalize_workspace_path(workspace)?;
        let read_only_sid = self.read_only_sid()?;
        let write_sid = self.write_sid(&workspace)?;
        let receipt = SandboxSetupReceipt {
            version: SETUP_RECEIPT_VERSION,
            helper_version: helper_version.to_string(),
            workspace: workspace.to_string_lossy().into_owned(),
            read_only_sid,
            write_sid,
        };
        let bytes = serde_json::to_vec_pretty(&receipt)?;
        let receipt_path = self.receipt_path(&workspace)?;
        atomic_write(&receipt_path, &bytes, AtomicWritePolicy::NoFollow)?;
        let legacy_path = self.root.join(SETUP_RECEIPT_FILE);
        if let Ok(legacy) = fs::read(&legacy_path)
            && let Ok(legacy_receipt) = serde_json::from_slice::<SandboxSetupReceipt>(&legacy)
        {
            let legacy_key = PathIdentity::windows(&legacy_receipt.workspace)?.storage_key();
            let workspace_key = PathIdentity::windows(&workspace.to_string_lossy())?.storage_key();
            if legacy_key == workspace_key {
                let _ = fs::remove_file(legacy_path);
            }
        }
        Ok(receipt)
    }

    /// Reconcile a missing setup receipt without replacing an existing
    /// capability identity. A receipt for another workspace or helper version
    /// is left in place and rejected so repair cannot silently widen access.
    pub fn repair_setup(
        &self,
        workspace: &Path,
        helper_version: &str,
    ) -> Result<SandboxSetupReceipt, WindowsSandboxError> {
        let workspace = normalize_workspace_path(workspace)?;
        if let Some((receipt, _path)) = self.read_setup_receipt_for_workspace(&workspace)? {
            validate_receipt(&receipt, helper_version)?;
            let requested_key = PathIdentity::windows(&workspace.to_string_lossy())?.storage_key();
            let receipt_key = PathIdentity::windows(&receipt.workspace)?.storage_key();
            if requested_key != receipt_key {
                return Err(WindowsSandboxError::InvalidState(
                    "Windows sandbox setup receipt belongs to another workspace".to_string(),
                ));
            }
        }
        self.provision_setup(&workspace, helper_version)
    }

    /// Revoke one workspace capability and remove its setup receipt. Removal
    /// is idempotent and deliberately keeps the shared read-only identity.
    pub fn remove_setup(
        &self,
        workspace: &Path,
        helper_version: &str,
    ) -> Result<bool, WindowsSandboxError> {
        let workspace = normalize_workspace_path(workspace)?;
        let Some((receipt, receipt_path)) = self.read_setup_receipt_for_workspace(&workspace)?
        else {
            return Ok(false);
        };
        validate_receipt(&receipt, helper_version)?;
        let workspace_key = PathIdentity::windows(&workspace.to_string_lossy())?.storage_key();
        let receipt_key = PathIdentity::windows(&receipt.workspace)?.storage_key();
        if workspace_key != receipt_key {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt belongs to another workspace".to_string(),
            ));
        }

        let _lock = ExclusiveFileLock::acquire(&self.root.join("capabilities.lock"))?;
        let state_path = self.root.join(CAPABILITY_STATE_FILE);
        let state = match fs::read(&state_path) {
            Ok(bytes) => {
                let state = serde_json::from_slice::<CapabilityState>(&bytes)?;
                validate_state(&state)?;
                Some(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        if let Some(mut state) = state {
            match state.write_root_sids.get(&workspace_key) {
                Some(sid) if sid != &receipt.write_sid => {
                    return Err(WindowsSandboxError::InvalidState(
                        "Windows sandbox setup receipt does not match the workspace capability"
                            .to_string(),
                    ));
                }
                Some(_) => {
                    state.write_root_sids.remove(&workspace_key);
                    persist_state(&state_path, &state)?;
                }
                None => {
                    // The state update may have succeeded before process loss;
                    // only the stale receipt remains to clean up.
                }
            }
        }

        match fs::remove_file(&receipt_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Validate setup state without creating or mutating capability entries.
    pub fn verify_setup(
        &self,
        helper_version: &str,
    ) -> Result<SandboxSetupReceipt, WindowsSandboxError> {
        let receipt = self.read_setup_receipt()?.ok_or_else(|| {
            WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt is missing; run the setup helper".to_string(),
            )
        })?;
        validate_receipt(&receipt, helper_version)?;
        let state_path = self.root.join(CAPABILITY_STATE_FILE);
        let state = serde_json::from_slice::<CapabilityState>(&fs::read(state_path)?)?;
        validate_state(&state)?;
        if state.read_only_sid != receipt.read_only_sid {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt does not match capability state".to_string(),
            ));
        }
        let workspace_key = PathIdentity::windows(&receipt.workspace)?.storage_key();
        if state.write_root_sids.get(&workspace_key) != Some(&receipt.write_sid) {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt does not match the workspace capability".to_string(),
            ));
        }
        Ok(receipt)
    }

    /// Validate setup and require that its receipt grants the requested
    /// workspace using Windows path identity semantics.
    pub fn verify_setup_for_workspace(
        &self,
        workspace: &Path,
        helper_version: &str,
    ) -> Result<SandboxSetupReceipt, WindowsSandboxError> {
        let workspace = normalize_workspace_path(workspace)?;
        let receipt = self
            .read_setup_receipt_for_workspace(&workspace)?
            .map(|(receipt, _)| receipt)
            .ok_or_else(|| {
                WindowsSandboxError::InvalidState(
                    "Windows sandbox setup receipt is missing; run the setup helper".to_string(),
                )
            })?;
        validate_receipt(&receipt, helper_version)?;
        self.verify_receipt_state(&receipt)?;
        let requested_key = PathIdentity::windows(&workspace.to_string_lossy())?.storage_key();
        let receipt_key = PathIdentity::windows(&receipt.workspace)?.storage_key();
        if requested_key != receipt_key {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt belongs to another workspace".to_string(),
            ));
        }
        Ok(receipt)
    }

    fn read_setup_receipt(&self) -> Result<Option<SandboxSetupReceipt>, WindowsSandboxError> {
        match fs::read(self.root.join(SETUP_RECEIPT_FILE)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn read_setup_receipt_for_workspace(
        &self,
        workspace: &Path,
    ) -> Result<Option<(SandboxSetupReceipt, PathBuf)>, WindowsSandboxError> {
        let workspace_path = self.receipt_path(workspace)?;
        match fs::read(&workspace_path) {
            Ok(bytes) => Ok(Some((serde_json::from_slice(&bytes)?, workspace_path))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let legacy_path = self.root.join(SETUP_RECEIPT_FILE);
                match fs::read(&legacy_path) {
                    Ok(bytes) => Ok(Some((serde_json::from_slice(&bytes)?, legacy_path))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn receipt_path(&self, workspace: &Path) -> Result<PathBuf, WindowsSandboxError> {
        let identity = PathIdentity::windows(&workspace.to_string_lossy())?;
        let digest = Sha256::digest(identity.storage_key().as_bytes());
        let suffix = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(self.root.join(format!("setup-receipt-{suffix}.json")))
    }

    fn verify_receipt_state(
        &self,
        receipt: &SandboxSetupReceipt,
    ) -> Result<(), WindowsSandboxError> {
        let state_path = self.root.join(CAPABILITY_STATE_FILE);
        let state = serde_json::from_slice::<CapabilityState>(&fs::read(state_path)?)?;
        validate_state(&state)?;
        if state.read_only_sid != receipt.read_only_sid {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt does not match capability state".to_string(),
            ));
        }
        let workspace_key = PathIdentity::windows(&receipt.workspace)?.storage_key();
        if state.write_root_sids.get(&workspace_key) != Some(&receipt.write_sid) {
            return Err(WindowsSandboxError::InvalidState(
                "Windows sandbox setup receipt does not match the workspace capability".to_string(),
            ));
        }
        Ok(())
    }

    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut CapabilityState) -> Result<(T, bool), WindowsSandboxError>,
    ) -> Result<T, WindowsSandboxError> {
        fs::create_dir_all(&self.root)?;
        let _lock = ExclusiveFileLock::acquire(&self.root.join("capabilities.lock"))?;
        let path = self.root.join("capabilities.json");
        let (mut state, created) = load_state(&path)?;
        let (value, changed) = operation(&mut state)?;
        if created || changed {
            persist_state(&path, &state)?;
        }
        Ok(value)
    }
}

// Windows canonicalization may switch a path to the extended-length spelling
// or resolve an 8.3 alias. Capability receipts must use the same object path
// that the launch adapter verifies, while synthetic contract fixtures may use
// paths that do not exist on the host.
fn normalize_workspace_path(path: &Path) -> Result<PathBuf, WindowsSandboxError> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error.into()),
    }
}

fn validate_receipt(
    receipt: &SandboxSetupReceipt,
    helper_version: &str,
) -> Result<(), WindowsSandboxError> {
    if receipt.version != SETUP_RECEIPT_VERSION
        || receipt.helper_version != helper_version
        || !valid_capability_sid(&receipt.read_only_sid)
        || !valid_capability_sid(&receipt.write_sid)
        || receipt.workspace.is_empty()
    {
        return Err(WindowsSandboxError::InvalidState(
            "Windows sandbox setup receipt is invalid or belongs to another helper version"
                .to_string(),
        ));
    }
    Ok(())
}

fn load_state(path: &Path) -> Result<(CapabilityState, bool), WindowsSandboxError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((new_state(), true));
        }
        Err(error) => return Err(error.into()),
    };
    let state: CapabilityState = serde_json::from_slice(&bytes)?;
    validate_state(&state)?;
    Ok((state, false))
}

fn validate_state(state: &CapabilityState) -> Result<(), WindowsSandboxError> {
    if state.version != CAPABILITY_STATE_VERSION {
        return Err(WindowsSandboxError::InvalidState(format!(
            "unsupported capability state version {}",
            state.version
        )));
    }
    if !valid_capability_sid(&state.read_only_sid)
        || state
            .write_root_sids
            .values()
            .any(|sid| !valid_capability_sid(sid))
    {
        return Err(WindowsSandboxError::InvalidState(
            "capability state contains an invalid SID".to_string(),
        ));
    }
    Ok(())
}

fn persist_state(path: &Path, state: &CapabilityState) -> Result<(), WindowsSandboxError> {
    let bytes = serde_json::to_vec_pretty(state)?;
    atomic_write(path, &bytes, AtomicWritePolicy::NoFollow)?;
    Ok(())
}

fn new_state() -> CapabilityState {
    CapabilityState {
        version: CAPABILITY_STATE_VERSION,
        read_only_sid: new_capability_sid(),
        write_root_sids: BTreeMap::new(),
    }
}

fn new_capability_sid() -> String {
    let bytes = *Uuid::new_v4().as_bytes();
    let parts = bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("UUID chunk")))
        .collect::<Vec<_>>();
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        parts[0], parts[1], parts[2], parts[3]
    )
}

fn valid_capability_sid(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("S-1-5-21-") else {
        return false;
    };
    let parts = rest.split('-').collect::<Vec<_>>();
    parts.len() == 4 && parts.iter().all(|part| part.parse::<u32>().is_ok())
}
