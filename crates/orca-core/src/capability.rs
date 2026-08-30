use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::approval_types::ApprovalMode;

/// The execution class is part of the security boundary, not a UI label.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProcessClass {
    SandboxedTool,
    UserTrustedIntegration,
    WorkflowWorker,
    RemoteSandbox,
}

/// Whether the selected platform backend actually enforces the capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementState {
    Enforced,
    Advisory,
    Unavailable,
}

/// Authority-bearing denial evidence emitted by a sandbox backend. This is
/// intentionally distinct from stderr diagnostics, which are process
/// controlled and therefore never suitable for permission escalation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxDenialSource {
    Kernel,
    Backend,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDenialReceipt {
    pub request_id: String,
    pub source: SandboxDenialSource,
    pub backend: String,
    pub denied_path: Option<PathBuf>,
    pub operation: String,
}

impl SandboxDenialReceipt {
    pub fn new(
        request_id: impl Into<String>,
        source: SandboxDenialSource,
        backend: impl Into<String>,
        denied_path: Option<PathBuf>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            source,
            backend: backend.into(),
            denied_path,
            operation: operation.into(),
        }
    }
}

/// Independent capability bits. `intersect` is the only operation used when
/// deriving a child or turn capability, so callers cannot accidentally widen
/// a parent grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilitySet {
    pub read: bool,
    pub write: bool,
    pub metadata_write: bool,
    pub network: bool,
    pub shell: bool,
    pub agent: bool,
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::read_only()
    }
}

/// Model/tool supplied capability intent. It is data only; callers must pass
/// it through [`EffectiveCapability::resolve`] before execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityRequest {
    pub request_id: String,
    pub process_class: CapabilityProcessClass,
    pub capabilities: CapabilitySet,
    pub cwd: PathBuf,
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub metadata_roots: Vec<PathBuf>,
    #[serde(default)]
    pub denied_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_targets: BTreeSet<String>,
    #[serde(default)]
    pub config_digest: Option<String>,
}

/// Capability limits supplied by the owning runtime. `None` means that the
/// dimension is not additionally bounded by this ceiling; `Some` is an
/// explicit allow-list and is intersected with the request.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityCeiling {
    pub capabilities: CapabilitySet,
    #[serde(default)]
    pub read_roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub write_roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub metadata_roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub denied_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_targets: Option<BTreeSet<String>>,
}

impl From<CapabilitySet> for CapabilityCeiling {
    fn from(capabilities: CapabilitySet) -> Self {
        Self {
            capabilities,
            ..Self::default()
        }
    }
}

impl CapabilityRequest {
    pub fn new(
        request_id: impl Into<String>,
        process_class: CapabilityProcessClass,
        capabilities: CapabilitySet,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            process_class,
            capabilities,
            cwd: cwd.into(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            metadata_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_targets: BTreeSet::new(),
            config_digest: None,
        }
    }
}

/// Immutable capability materialized for one operation. It can only be
/// created by the resolver, so launchers never receive an unbounded request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectiveCapability {
    pub request_id: String,
    pub process_class: CapabilityProcessClass,
    pub capabilities: CapabilitySet,
    pub cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub metadata_roots: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
    pub network_targets: BTreeSet<String>,
    pub config_digest: Option<String>,
}

impl EffectiveCapability {
    pub fn resolve(
        request: CapabilityRequest,
        ceiling: impl Into<CapabilityCeiling>,
        mode: ApprovalMode,
    ) -> Result<Self, CapabilityError> {
        if request.process_class == CapabilityProcessClass::UserTrustedIntegration {
            return Err(CapabilityError::UntrustedProcessClass);
        }
        Self::resolve_inner(request, ceiling, mode)
    }

    /// Resolve an explicitly user-owned integration. This is deliberately a
    /// separate API so model/tool-provided requests cannot self-label as a
    /// trusted process class and bypass the fail-closed broker gate.
    pub fn resolve_user_trusted(
        request: CapabilityRequest,
        ceiling: impl Into<CapabilityCeiling>,
        mode: ApprovalMode,
    ) -> Result<Self, CapabilityError> {
        if request.process_class != CapabilityProcessClass::UserTrustedIntegration {
            return Err(CapabilityError::UntrustedProcessClass);
        }
        Self::resolve_inner(request, ceiling, mode)
    }

    fn resolve_inner(
        request: CapabilityRequest,
        ceiling: impl Into<CapabilityCeiling>,
        mode: ApprovalMode,
    ) -> Result<Self, CapabilityError> {
        validate_absolute_path(&request.cwd)?;
        for root in request
            .read_roots
            .iter()
            .chain(request.write_roots.iter())
            .chain(request.metadata_roots.iter())
            .chain(request.denied_roots.iter())
        {
            validate_absolute_path(root)?;
        }
        let ceiling = ceiling.into();
        for root in ceiling
            .read_roots
            .iter()
            .flatten()
            .chain(ceiling.write_roots.iter().flatten())
            .chain(ceiling.metadata_roots.iter().flatten())
            .chain(ceiling.denied_roots.iter())
        {
            validate_absolute_path(root)?;
        }
        let mode_ceiling = CapabilitySet::for_approval_mode(mode);
        if mode == ApprovalMode::Plan {
            request
                .capabilities
                .ensure_subset_of(&mode_ceiling)
                .map_err(|_| CapabilityError::PlanViolation)?;
        }
        let capabilities = request
            .capabilities
            .intersect(&ceiling.capabilities)?
            .intersect(&mode_ceiling)?;
        let read_roots = intersect_roots(request.read_roots, ceiling.read_roots.as_deref());
        let write_roots = intersect_roots(request.write_roots, ceiling.write_roots.as_deref());
        let metadata_roots =
            intersect_roots(request.metadata_roots, ceiling.metadata_roots.as_deref());
        let network_targets = match ceiling.network_targets {
            None => request.network_targets,
            Some(allowed) => request
                .network_targets
                .intersection(&allowed)
                .cloned()
                .collect(),
        };
        Ok(Self {
            request_id: request.request_id,
            process_class: request.process_class,
            capabilities,
            cwd: request.cwd,
            read_roots,
            write_roots,
            metadata_roots,
            denied_roots: merge_denied_roots(request.denied_roots, ceiling.denied_roots),
            network_targets,
            config_digest: request.config_digest,
        })
    }

    pub fn receipt(
        &self,
        enforcement: EnforcementState,
        backend: impl Into<String>,
    ) -> CapabilityReceipt {
        let mut receipt = CapabilityReceipt::new(
            self.request_id.clone(),
            self.process_class,
            enforcement,
            self.cwd.clone(),
            backend,
        );
        receipt.capabilities = self.capabilities.clone();
        receipt.read_roots = self.read_roots.clone();
        receipt.write_roots = self.write_roots.clone();
        receipt.metadata_roots = self.metadata_roots.clone();
        receipt.denied_roots = self.denied_roots.clone();
        receipt.network_targets = self.network_targets.clone();
        receipt.provenance = self.config_digest.clone();
        receipt
    }

    /// Verify that a materialized capability stays within the owning runtime's
    /// hard ceiling. This check deliberately covers roots and target sets in
    /// addition to the boolean capability bits.
    pub fn ensure_subset_of(&self, ceiling: &CapabilityCeiling) -> Result<(), CapabilityError> {
        self.capabilities.ensure_subset_of(&ceiling.capabilities)?;
        ensure_roots_subset(&self.read_roots, ceiling.read_roots.as_deref())?;
        ensure_roots_subset(&self.write_roots, ceiling.write_roots.as_deref())?;
        ensure_roots_subset(&self.metadata_roots, ceiling.metadata_roots.as_deref())?;
        if let Some(allowed) = ceiling.network_targets.as_ref()
            && !self.network_targets.is_subset(allowed)
        {
            return Err(CapabilityError::Widening);
        }
        Ok(())
    }
}

fn intersect_roots(requested: Vec<PathBuf>, ceiling: Option<&[PathBuf]>) -> Vec<PathBuf> {
    let Some(ceiling) = ceiling else {
        return requested;
    };
    let mut effective = Vec::new();
    for requested_root in requested {
        for ceiling_root in ceiling {
            if requested_root.starts_with(ceiling_root) {
                effective.push(requested_root.clone());
            } else if ceiling_root.starts_with(&requested_root) {
                effective.push(ceiling_root.clone());
            }
        }
    }
    effective.sort();
    effective.dedup();
    effective
}

fn ensure_roots_subset(
    requested: &[PathBuf],
    ceiling: Option<&[PathBuf]>,
) -> Result<(), CapabilityError> {
    let Some(ceiling) = ceiling else {
        return Ok(());
    };
    if requested.iter().all(|requested_root| {
        ceiling
            .iter()
            .any(|ceiling_root| requested_root.starts_with(ceiling_root))
    }) {
        Ok(())
    } else {
        Err(CapabilityError::Widening)
    }
}

fn merge_denied_roots(mut requested: Vec<PathBuf>, ceiling: Vec<PathBuf>) -> Vec<PathBuf> {
    requested.extend(ceiling);
    requested.sort();
    requested.dedup();
    requested
}

impl CapabilitySet {
    pub const fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            metadata_write: false,
            network: false,
            shell: false,
            agent: false,
        }
    }

    pub const fn workspace_write() -> Self {
        Self {
            read: true,
            write: true,
            metadata_write: false,
            network: false,
            shell: true,
            agent: true,
        }
    }

    pub const fn all() -> Self {
        Self {
            read: true,
            write: true,
            metadata_write: true,
            network: true,
            shell: true,
            agent: true,
        }
    }

    pub const fn for_approval_mode(mode: ApprovalMode) -> Self {
        match mode {
            ApprovalMode::Plan => Self::read_only(),
            ApprovalMode::Suggest | ApprovalMode::AutoEdit => Self::workspace_write(),
            ApprovalMode::FullAuto => Self::all(),
        }
    }

    pub fn intersect(&self, other: &Self) -> Result<Self, CapabilityError> {
        Ok(Self {
            read: self.read && other.read,
            write: self.write && other.write,
            metadata_write: self.metadata_write && other.metadata_write,
            network: self.network && other.network,
            shell: self.shell && other.shell,
            agent: self.agent && other.agent,
        })
    }

    pub fn ensure_subset_of(&self, parent: &Self) -> Result<(), CapabilityError> {
        if (!parent.read && self.read)
            || (!parent.write && self.write)
            || (!parent.metadata_write && self.metadata_write)
            || (!parent.network && self.network)
            || (!parent.shell && self.shell)
            || (!parent.agent && self.agent)
        {
            return Err(CapabilityError::Widening);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    Widening,
    PlanViolation,
    InvalidPath,
    UntrustedProcessClass,
}

fn validate_absolute_path(path: &std::path::Path) -> Result<(), CapabilityError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(CapabilityError::InvalidPath);
    }
    Ok(())
}

/// Immutable record returned by every broker launch and surfaced to clients.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityReceipt {
    pub request_id: String,
    pub process_class: CapabilityProcessClass,
    pub enforcement: EnforcementState,
    pub cwd: PathBuf,
    pub backend: String,
    pub capabilities: CapabilitySet,
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub metadata_roots: Vec<PathBuf>,
    #[serde(default)]
    pub denied_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_targets: BTreeSet<String>,
    #[serde(default)]
    pub provenance: Option<String>,
}

impl CapabilityReceipt {
    pub fn new(
        request_id: impl Into<String>,
        process_class: CapabilityProcessClass,
        enforcement: EnforcementState,
        cwd: impl Into<PathBuf>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            process_class,
            enforcement,
            cwd: cwd.into(),
            backend: backend.into(),
            capabilities: CapabilitySet::read_only(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            metadata_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_targets: BTreeSet::new(),
            provenance: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::{
        CapabilityCeiling, CapabilityProcessClass, CapabilityReceipt, CapabilityRequest,
        CapabilitySet, EffectiveCapability, EnforcementState, SandboxDenialReceipt,
        SandboxDenialSource,
    };
    use crate::approval_types::ApprovalMode;

    fn test_workspace() -> PathBuf {
        std::env::current_dir()
            .expect("current directory")
            .join("orca-capability-test-workspace")
    }

    #[test]
    fn plan_is_a_hard_read_only_ceiling() {
        let requested = CapabilitySet::all();
        let effective = requested
            .intersect(&CapabilitySet::for_approval_mode(ApprovalMode::Plan))
            .expect("plan intersection");

        assert!(effective.read);
        assert!(!effective.write);
        assert!(!effective.metadata_write);
        assert!(!effective.network);
        assert!(!effective.shell);
        assert!(!effective.agent);
    }

    #[test]
    fn child_capability_cannot_widen_parent() {
        let parent = CapabilitySet::workspace_write();
        let child = CapabilitySet::all();

        assert!(child.ensure_subset_of(&parent).is_err());
        assert!(CapabilitySet::read_only().ensure_subset_of(&parent).is_ok());
    }

    #[test]
    fn receipt_round_trips_enforcement_and_process_class() {
        let receipt = CapabilityReceipt::new(
            "req-1",
            CapabilityProcessClass::SandboxedTool,
            EnforcementState::Enforced,
            test_workspace(),
            "landlock+seccomp",
        );
        let encoded = serde_json::to_string(&receipt).expect("serialize receipt");
        let decoded: CapabilityReceipt =
            serde_json::from_str(&encoded).expect("deserialize receipt");

        assert_eq!(decoded, receipt);
    }

    #[test]
    fn sandbox_denial_receipt_is_structured_and_source_bound() {
        let receipt = SandboxDenialReceipt::new(
            "req-denied",
            SandboxDenialSource::Kernel,
            "landlock",
            Some(test_workspace().join(".git/index.lock")),
            "write",
        );
        let encoded = serde_json::to_string(&receipt).expect("serialize denial receipt");
        let decoded: SandboxDenialReceipt =
            serde_json::from_str(&encoded).expect("deserialize denial receipt");

        assert_eq!(decoded, receipt);
        assert_eq!(decoded.source, SandboxDenialSource::Kernel);
    }

    #[test]
    fn effective_capability_is_the_intersection_of_request_and_ceiling() {
        let request = CapabilityRequest::new(
            "req-2",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::all(),
            test_workspace(),
        );
        let effective = EffectiveCapability::resolve(
            request,
            CapabilitySet::workspace_write(),
            ApprovalMode::AutoEdit,
        )
        .expect("resolve effective capability");

        assert_eq!(effective.capabilities, CapabilitySet::workspace_write());
        assert_eq!(effective.request_id, "req-2");
    }

    #[test]
    fn plan_resolution_rejects_non_read_requests_instead_of_silently_downgrading() {
        let request = CapabilityRequest::new(
            "req-3",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::workspace_write(),
            test_workspace(),
        );

        assert!(
            EffectiveCapability::resolve(request, CapabilitySet::all(), ApprovalMode::Plan)
                .is_err()
        );
    }

    #[test]
    fn effective_capability_clips_requested_roots_to_ceiling() {
        let mut request = CapabilityRequest::new(
            "req-roots",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::workspace_write(),
            test_workspace(),
        );
        let workspace = test_workspace();
        let source = workspace.join("src");
        let outside = workspace
            .parent()
            .expect("workspace parent")
            .join("orca-capability-test-outside");
        request.read_roots = vec![source.clone(), outside];
        request.write_roots = vec![workspace.clone()];
        let ceiling = super::CapabilityCeiling {
            capabilities: CapabilitySet::workspace_write(),
            read_roots: Some(vec![workspace]),
            write_roots: Some(vec![source.clone()]),
            metadata_roots: None,
            denied_roots: Vec::new(),
            network_targets: None,
        };

        let effective = EffectiveCapability::resolve(request, ceiling, ApprovalMode::AutoEdit)
            .expect("resolve roots");

        assert_eq!(effective.read_roots, vec![source.clone()]);
        assert_eq!(effective.write_roots, vec![source]);
    }

    #[test]
    fn final_capability_subset_check_covers_roots_and_network_targets() {
        let mut request = CapabilityRequest::new(
            "req-final-subset",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::workspace_write(),
            test_workspace(),
        );
        let workspace = test_workspace();
        let source = workspace.join("src");
        request.read_roots = vec![source.clone()];
        request.write_roots = vec![source.clone()];
        request
            .network_targets
            .insert("api.example.com".to_string());
        let effective = EffectiveCapability::resolve(
            request,
            CapabilityCeiling {
                capabilities: CapabilitySet::workspace_write(),
                read_roots: Some(vec![workspace.clone()]),
                write_roots: Some(vec![workspace.clone()]),
                metadata_roots: None,
                denied_roots: Vec::new(),
                network_targets: Some(["api.example.com".to_string()].into_iter().collect()),
            },
            ApprovalMode::AutoEdit,
        )
        .expect("resolve final subset");

        let mut narrowed = CapabilityCeiling {
            capabilities: CapabilitySet::workspace_write(),
            read_roots: Some(vec![source]),
            write_roots: Some(vec![workspace.join("src")]),
            metadata_roots: None,
            denied_roots: Vec::new(),
            network_targets: Some(BTreeSet::new()),
        };
        assert!(effective.ensure_subset_of(&narrowed).is_err());
        narrowed.network_targets = Some(["api.example.com".to_string()].into_iter().collect());
        assert!(effective.ensure_subset_of(&narrowed).is_ok());
    }

    #[test]
    fn effective_capability_intersects_network_targets() {
        let mut request = CapabilityRequest::new(
            "req-network",
            CapabilityProcessClass::UserTrustedIntegration,
            CapabilitySet::all(),
            test_workspace(),
        );
        request.network_targets = ["api.example.com", "evil.example.com"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let ceiling = super::CapabilityCeiling {
            capabilities: CapabilitySet::all(),
            read_roots: None,
            write_roots: None,
            metadata_roots: None,
            denied_roots: Vec::new(),
            network_targets: Some(["api.example.com".to_string()].into_iter().collect()),
        };

        let effective =
            EffectiveCapability::resolve_user_trusted(request, ceiling, ApprovalMode::FullAuto)
                .expect("resolve network");

        assert_eq!(
            effective.network_targets,
            ["api.example.com".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn capability_resolution_rejects_relative_paths() {
        let request = CapabilityRequest::new(
            "req-relative",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::read_only(),
            "relative/workspace",
        );
        assert_eq!(
            EffectiveCapability::resolve(request, CapabilitySet::read_only(), ApprovalMode::Plan),
            Err(super::CapabilityError::InvalidPath)
        );
    }

    #[test]
    fn capability_resolution_rejects_parent_directory_paths() {
        let request = CapabilityRequest::new(
            "req-parent",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::read_only(),
            format!("{}/../outside", test_workspace().display()),
        );

        assert_eq!(
            EffectiveCapability::resolve(request, CapabilitySet::read_only(), ApprovalMode::Plan),
            Err(super::CapabilityError::InvalidPath)
        );
    }

    #[test]
    fn ordinary_resolution_rejects_a_self_declared_trusted_process() {
        let request = CapabilityRequest::new(
            "req-trusted-spoof",
            CapabilityProcessClass::UserTrustedIntegration,
            CapabilitySet::read_only(),
            test_workspace(),
        );

        assert_eq!(
            EffectiveCapability::resolve(request, CapabilitySet::all(), ApprovalMode::FullAuto),
            Err(super::CapabilityError::UntrustedProcessClass)
        );
    }

    #[test]
    fn trusted_resolution_requires_the_trusted_process_class() {
        let request = CapabilityRequest::new(
            "req-not-trusted",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::read_only(),
            test_workspace(),
        );

        assert_eq!(
            EffectiveCapability::resolve_user_trusted(
                request,
                CapabilitySet::all(),
                ApprovalMode::FullAuto,
            ),
            Err(super::CapabilityError::UntrustedProcessClass)
        );
    }
}
