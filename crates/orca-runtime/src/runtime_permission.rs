use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::PermissionProfileNetworkAccess;

use crate::network_proxy::RuntimeNetworkBlockReport;
use crate::protocol::{
    PermissionGrantScope, PermissionResponseDecision, RequestNetworkPermissions,
    RequestPermissionProfile,
};

/// Caller-proven provenance for a permission prompt. The actor binds a
/// foreground request to the committed tool identity. Child requests must
/// carry the complete activity owner, so they cannot be recovered from an
/// incidental snapshot match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePermissionContext {
    Foreground {
        origin: crate::surface::SurfacePermissionOrigin,
    },
    Child {
        task_id: crate::surface::SurfaceTaskId,
        task_revision: crate::surface::TaskRevision,
        agent_id: crate::surface::SurfaceSubagentId,
        agent_revision: crate::surface::SubagentRevision,
        activity_id: crate::surface::SurfaceActivityId,
        turn_id: crate::surface::SurfaceTurnId,
        tool_call_id: crate::surface::SurfaceToolCallId,
        origin: crate::surface::SurfacePermissionOrigin,
    },
}

impl RuntimePermissionContext {
    pub const fn foreground(origin: crate::surface::SurfacePermissionOrigin) -> Self {
        Self::Foreground { origin }
    }

    pub fn child(
        task_id: crate::surface::SurfaceTaskId,
        task_revision: crate::surface::TaskRevision,
        agent_id: crate::surface::SurfaceSubagentId,
        agent_revision: crate::surface::SubagentRevision,
        activity_id: crate::surface::SurfaceActivityId,
        turn_id: crate::surface::SurfaceTurnId,
        tool_call_id: crate::surface::SurfaceToolCallId,
    ) -> Self {
        Self::Child {
            task_id,
            task_revision,
            agent_id,
            agent_revision,
            activity_id,
            turn_id,
            tool_call_id,
            origin: crate::surface::SurfacePermissionOrigin::ChildAgent,
        }
    }
}

pub(crate) const SESSION_METADATA_DIRECTORY_SOURCE: &str = "session-metadata";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionRequest {
    pub id: String,
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
    /// Caller-proven owner identity. The runtime actor verifies it against the
    /// committed tool and active operation before publishing it.
    pub context: RuntimePermissionContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionResponse {
    pub decision: PermissionResponseDecision,
    pub scope: PermissionGrantScope,
    pub permissions: RequestPermissionProfile,
    pub strict_auto_review: bool,
}

pub trait RuntimePermissionRequestHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse>;

    /// Request a permission at a caller-proven pre-side-effect checkpoint.
    /// Implementations that do not own durable recovery may use the ordinary
    /// request path; runtime-owned handlers can persist the supplied overlay.
    fn request_permissions_pre_side_effect(
        &self,
        request: &RuntimePermissionRequest,
        _permission_overlay: &TurnPermissionOverlay,
    ) -> io::Result<RuntimePermissionResponse> {
        self.request_permissions(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimePermissionPromptDecision {
    AutoAllow,
    Prompt,
    Reject { reason: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePermissionOrigin {
    Bash,
    CommandExec,
}

impl RuntimePermissionOrigin {
    fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::CommandExec => "command/exec",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePermissionRequestKind {
    NetworkBlock,
    CapabilityBoundary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionDecision {
    pub origin: RuntimePermissionOrigin,
    pub kind: RuntimePermissionRequestKind,
    pub request: RuntimePermissionRequest,
}

impl RuntimePermissionDecision {
    pub fn into_request(self) -> RuntimePermissionRequest {
        self.request
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimePermissionEvaluation {
    Request(RuntimePermissionDecision),
    Deny {
        origin: RuntimePermissionOrigin,
        kind: RuntimePermissionRequestKind,
        reason: String,
    },
}

pub struct RuntimePermissionPolicy;

impl RuntimePermissionPolicy {
    pub(crate) fn decide_request_permissions_prompt(
        approval_mode: ApprovalMode,
        handler_available: bool,
    ) -> RuntimePermissionPromptDecision {
        if approval_mode == ApprovalMode::FullAuto {
            return RuntimePermissionPromptDecision::AutoAllow;
        }
        if handler_available {
            return RuntimePermissionPromptDecision::Prompt;
        }
        RuntimePermissionPromptDecision::Reject {
            reason: "request_permissions requires a runtime permission handler unless approval mode is full-auto",
        }
    }

    pub fn network_block_decision(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        block: &RuntimeNetworkBlockReport,
    ) -> Option<RuntimePermissionDecision> {
        match Self::network_block_evaluation(request_id, origin, block) {
            RuntimePermissionEvaluation::Request(decision) => Some(decision),
            RuntimePermissionEvaluation::Deny { .. } => None,
        }
    }

    pub fn network_block_evaluation(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        block: &RuntimeNetworkBlockReport,
    ) -> RuntimePermissionEvaluation {
        if block.error == "blocked-by-denylist" {
            return RuntimePermissionEvaluation::Deny {
                origin,
                kind: RuntimePermissionRequestKind::NetworkBlock,
                reason: format!(
                    "{} network access to {} was denied by configured network policy",
                    origin.label(),
                    block.host
                ),
            };
        }

        let mut domains = HashMap::new();
        domains.insert(block.host.clone(), PermissionProfileNetworkAccess::Allow);
        RuntimePermissionEvaluation::Request(RuntimePermissionDecision {
            origin,
            kind: RuntimePermissionRequestKind::NetworkBlock,
            request: RuntimePermissionRequest {
                id: request_id.to_string(),
                reason: Some(format!(
                    "{} attempted network access to {} ({})",
                    origin.label(),
                    block.host,
                    block.error
                )),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: Some(RequestNetworkPermissions {
                        enabled: None,
                        domains,
                    }),
                },
                context: RuntimePermissionContext::foreground(match origin {
                    RuntimePermissionOrigin::Bash => crate::surface::SurfacePermissionOrigin::Bash,
                    RuntimePermissionOrigin::CommandExec => {
                        crate::surface::SurfacePermissionOrigin::CommandExec
                    }
                }),
            },
        })
    }
}

pub(crate) struct AllowRequestedPermissions;

impl RuntimePermissionRequestHandler for AllowRequestedPermissions {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        Ok(RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnPermissionOverlay {
    additional_working_directories: Vec<PathBuf>,
    metadata_writable_directories: Vec<PathBuf>,
    network_domain_permissions:
        std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
    strict_auto_review: bool,
    preapproved_tool_call_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnPermissionOverlayDelta {
    additional_working_directories: Vec<PathBuf>,
    metadata_writable_directories: Vec<PathBuf>,
    network_domain_permissions:
        std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
    strict_auto_review: bool,
}

impl TurnPermissionOverlayDelta {
    pub fn additional_working_directories(&self) -> &[PathBuf] {
        &self.additional_working_directories
    }

    pub fn metadata_writable_directories(&self) -> &[PathBuf] {
        &self.metadata_writable_directories
    }

    pub fn network_domain_permissions(
        &self,
    ) -> &std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
    }

    pub fn strict_auto_review(&self) -> bool {
        self.strict_auto_review
    }
}

impl TurnPermissionOverlay {
    pub fn additional_working_directories(&self) -> &[PathBuf] {
        &self.additional_working_directories
    }

    pub fn metadata_writable_directories(&self) -> &[PathBuf] {
        &self.metadata_writable_directories
    }

    pub fn network_domain_permissions(
        &self,
    ) -> &std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
    }

    pub fn strict_auto_review(&self) -> bool {
        self.strict_auto_review
    }

    pub(crate) fn set_preapproved_tool_call_id(&mut self, id: Option<String>) {
        self.preapproved_tool_call_id = id;
    }

    pub(crate) fn consume_preapproved_tool_call_id(&mut self, id: &str) -> bool {
        if self.preapproved_tool_call_id.as_deref() != Some(id) {
            return false;
        }
        self.preapproved_tool_call_id = None;
        true
    }

    pub fn merge(&mut self, other: &Self) {
        for root in &other.additional_working_directories {
            if !self.additional_working_directories.contains(root) {
                self.additional_working_directories.push(root.clone());
            }
        }
        for root in &other.metadata_writable_directories {
            if !self.metadata_writable_directories.contains(root) {
                self.metadata_writable_directories.push(root.clone());
            }
        }
        for (domain, access) in &other.network_domain_permissions {
            self.network_domain_permissions
                .insert(domain.clone(), *access);
        }
        self.strict_auto_review |= other.strict_auto_review;
    }

    pub(crate) fn delta_from(&self, baseline: &Self) -> TurnPermissionOverlayDelta {
        let additional_working_directories = self
            .additional_working_directories
            .iter()
            .filter(|root| !baseline.additional_working_directories.contains(root))
            .cloned()
            .collect();
        let metadata_writable_directories = self
            .metadata_writable_directories
            .iter()
            .filter(|root| !baseline.metadata_writable_directories.contains(root))
            .cloned()
            .collect();
        let network_domain_permissions = self
            .network_domain_permissions
            .iter()
            .filter(|(domain, access)| {
                baseline.network_domain_permissions.get(*domain) != Some(access)
            })
            .map(|(domain, access)| (domain.clone(), *access))
            .collect();
        TurnPermissionOverlayDelta {
            additional_working_directories,
            metadata_writable_directories,
            network_domain_permissions,
            strict_auto_review: self.strict_auto_review && !baseline.strict_auto_review,
        }
    }

    pub(crate) fn apply_delta(&mut self, delta: &TurnPermissionOverlayDelta) {
        for root in &delta.additional_working_directories {
            if !self.additional_working_directories.contains(root) {
                self.additional_working_directories.push(root.clone());
            }
        }
        for root in &delta.metadata_writable_directories {
            if !self.metadata_writable_directories.contains(root) {
                self.metadata_writable_directories.push(root.clone());
            }
        }
        for (domain, access) in &delta.network_domain_permissions {
            self.network_domain_permissions
                .insert(domain.clone(), *access);
        }
        self.strict_auto_review |= delta.strict_auto_review;
    }

    pub(crate) fn merge_network_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(network) = permissions.network.as_ref() {
            for (domain, access) in &network.domains {
                self.network_domain_permissions
                    .insert(domain.clone(), *access);
            }
        }
    }

    pub(crate) fn merge_strict_auto_review(&mut self, strict_auto_review: bool) {
        self.strict_auto_review |= strict_auto_review;
    }

    pub fn request_and_merge(
        &mut self,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let response = handler.request_permissions(&request)?;
        if response.decision == PermissionResponseDecision::Allow {
            self.merge_permissions(&response.permissions);
            self.merge_strict_auto_review(response.strict_auto_review);
        }
        Ok(response)
    }

    /// Function intent contract:
    ///
    /// - Input: a permission request whose caller has proven no external tool
    ///   side effect occurred, plus the current turn overlay.
    /// - Output: the normal response while preserving existing merge, scope,
    ///   and strict-auto-review behavior.
    /// - Errors: forwards handler errors without mutating the overlay.
    /// - State changes and external calls: the handler may durably checkpoint
    ///   the request; this overlay changes only after an allowed response.
    pub(crate) fn request_and_merge_pre_side_effect(
        &mut self,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let response = handler.request_permissions_pre_side_effect(&request, self)?;
        if response.decision == PermissionResponseDecision::Allow {
            self.merge_permissions(&response.permissions);
            self.merge_strict_auto_review(response.strict_auto_review);
        }
        Ok(response)
    }

    pub(crate) fn merge_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(file_system) = permissions.file_system.as_ref()
            && let Some(write_roots) = file_system.write.as_ref()
        {
            for root in write_roots {
                if root.as_os_str().is_empty() {
                    continue;
                }
                if orca_tools::sandbox::is_protected_metadata_root(root) {
                    if orca_tools::sandbox::is_safe_metadata_writable_root(root)
                        && !self.metadata_writable_directories.contains(root)
                    {
                        self.metadata_writable_directories.push(root.clone());
                    }
                } else if !self.additional_working_directories.contains(root) {
                    self.additional_working_directories.push(root.clone());
                }
            }
        }
        self.merge_network_permissions(permissions);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::PermissionProfileNetworkAccess;

    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::protocol::{RequestFileSystemPermissions, RequestPermissionProfile};

    use super::{
        RuntimePermissionEvaluation, RuntimePermissionOrigin, RuntimePermissionPolicy,
        RuntimePermissionPromptDecision, RuntimePermissionRequestKind, TurnPermissionOverlay,
    };

    #[test]
    fn preapproved_tool_call_id_is_consumed_once_for_exact_match_only() {
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("tool-1".to_string()));

        assert!(!overlay.consume_preapproved_tool_call_id("tool-2"));
        assert!(overlay.consume_preapproved_tool_call_id("tool-1"));
        assert!(!overlay.consume_preapproved_tool_call_id("tool-1"));
    }

    #[test]
    fn permission_overlay_only_carries_scoped_capabilities() {
        let baseline = TurnPermissionOverlay::default();
        let mut current = baseline.clone();
        current.merge_permissions(&RequestPermissionProfile {
            network: Some(crate::protocol::RequestNetworkPermissions {
                enabled: None,
                domains: HashMap::from([(
                    "api.example.com".to_string(),
                    PermissionProfileNetworkAccess::Allow,
                )]),
            }),
            ..Default::default()
        });

        let delta = current.delta_from(&baseline);

        let mut restored = baseline;
        restored.apply_delta(&delta);
        assert_eq!(
            restored.network_domain_permissions(),
            current.network_domain_permissions()
        );
    }

    #[test]
    fn approved_exact_metadata_roots_are_kept_separate() {
        let mut overlay = TurnPermissionOverlay::default();

        overlay.merge_permissions(&RequestPermissionProfile {
            file_system: Some(RequestFileSystemPermissions {
                write: Some(vec![
                    PathBuf::from("/repo/.git"),
                    PathBuf::from("/repo/.agents"),
                    PathBuf::from("/repo/.codex"),
                    PathBuf::from("/repo"),
                    PathBuf::from("/repo/.git/config"),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert_eq!(
            overlay.metadata_writable_directories(),
            &[
                PathBuf::from("/repo/.git"),
                PathBuf::from("/repo/.agents"),
                PathBuf::from("/repo/.codex"),
            ]
        );
        assert_eq!(
            overlay.additional_working_directories(),
            &[PathBuf::from("/repo"), PathBuf::from("/repo/.git/config"),]
        );
    }

    #[cfg(unix)]
    #[test]
    fn approved_symlink_metadata_root_is_not_made_writable() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("outside");
        let metadata_link = parent.path().join(".agents");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &metadata_link).unwrap();
        let mut overlay = TurnPermissionOverlay::default();

        overlay.merge_permissions(&RequestPermissionProfile {
            file_system: Some(RequestFileSystemPermissions {
                write: Some(vec![metadata_link]),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(overlay.metadata_writable_directories().is_empty());
        assert!(overlay.additional_working_directories().is_empty());
    }

    #[test]
    fn permission_overlay_delta_carries_only_worker_changes() {
        let baseline = TurnPermissionOverlay {
            additional_working_directories: vec![PathBuf::from("/existing")],
            metadata_writable_directories: vec![PathBuf::from("/repo/.git")],
            network_domain_permissions: HashMap::from([(
                "api.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            )]),
            strict_auto_review: false,
            preapproved_tool_call_id: Some("approval-only".to_string()),
        };
        let current = TurnPermissionOverlay {
            additional_working_directories: vec![PathBuf::from("/existing"), PathBuf::from("/new")],
            metadata_writable_directories: vec![
                PathBuf::from("/repo/.git"),
                PathBuf::from("/repo/.agents"),
            ],
            network_domain_permissions: HashMap::from([
                (
                    "api.example.com".to_string(),
                    PermissionProfileNetworkAccess::Allow,
                ),
                (
                    "blocked.example.com".to_string(),
                    PermissionProfileNetworkAccess::Deny,
                ),
            ]),
            strict_auto_review: true,
            preapproved_tool_call_id: None,
        };

        let delta = current.delta_from(&baseline);
        assert_eq!(
            delta.additional_working_directories(),
            &[PathBuf::from("/new")]
        );
        assert_eq!(
            delta.metadata_writable_directories(),
            &[PathBuf::from("/repo/.agents")]
        );
        assert_eq!(
            delta
                .network_domain_permissions()
                .get("blocked.example.com"),
            Some(&PermissionProfileNetworkAccess::Deny)
        );
        assert!(delta.strict_auto_review());

        let mut canonical = baseline.clone();
        canonical.apply_delta(&delta);
        assert_eq!(
            canonical.additional_working_directories(),
            &[PathBuf::from("/existing"), PathBuf::from("/new")]
        );
        assert_eq!(
            canonical.metadata_writable_directories(),
            &[PathBuf::from("/repo/.git"), PathBuf::from("/repo/.agents")]
        );
        assert_eq!(
            canonical
                .network_domain_permissions()
                .get("blocked.example.com"),
            Some(&PermissionProfileNetworkAccess::Deny)
        );
        assert!(canonical.strict_auto_review());
        assert_eq!(
            canonical.preapproved_tool_call_id.as_deref(),
            Some("approval-only"),
            "worker delta must not mutate approval-only canonical state"
        );
    }

    #[test]
    fn runtime_permission_policy_skips_denylist_network_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert!(
            RuntimePermissionPolicy::network_block_decision(
                "permission-1",
                RuntimePermissionOrigin::Bash,
                &block,
            )
            .is_none()
        );
    }

    #[test]
    fn runtime_permission_policy_explains_final_network_denials() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert_eq!(
            RuntimePermissionPolicy::network_block_evaluation(
                "permission-1",
                RuntimePermissionOrigin::Bash,
                &block,
            ),
            RuntimePermissionEvaluation::Deny {
                origin: RuntimePermissionOrigin::Bash,
                kind: RuntimePermissionRequestKind::NetworkBlock,
                reason:
                    "bash network access to blocked.orca.invalid was denied by configured network policy"
                        .to_string(),
            }
        );
    }

    #[test]
    fn runtime_permission_policy_builds_actor_scoped_network_decision() {
        let block = RuntimeNetworkBlockReport {
            host: "api.orca.invalid".to_string(),
            error: "blocked-by-allowlist",
        };

        let bash_decision = RuntimePermissionPolicy::network_block_decision(
            "permission-1",
            RuntimePermissionOrigin::Bash,
            &block,
        )
        .expect("bash network request");
        let command_decision = RuntimePermissionPolicy::network_block_decision(
            "permission-2",
            RuntimePermissionOrigin::CommandExec,
            &block,
        )
        .expect("command/exec network request");

        assert_eq!(bash_decision.origin, RuntimePermissionOrigin::Bash);
        assert_eq!(
            bash_decision.kind,
            RuntimePermissionRequestKind::NetworkBlock
        );
        assert_eq!(
            bash_decision.request.reason.as_deref(),
            Some("bash attempted network access to api.orca.invalid (blocked-by-allowlist)")
        );
        assert_eq!(
            command_decision.origin,
            RuntimePermissionOrigin::CommandExec
        );
        assert_eq!(
            command_decision.kind,
            RuntimePermissionRequestKind::NetworkBlock
        );
        assert_eq!(
            command_decision.request.reason.as_deref(),
            Some(
                "command/exec attempted network access to api.orca.invalid (blocked-by-allowlist)"
            )
        );
        assert_eq!(
            command_decision
                .request
                .permissions
                .network
                .as_ref()
                .and_then(|network| network.domains.get("api.orca.invalid")),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[test]
    fn runtime_permission_policy_decides_request_permissions_prompt_gate() {
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(
                ApprovalMode::FullAuto,
                false
            ),
            RuntimePermissionPromptDecision::AutoAllow
        );
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(ApprovalMode::Suggest, true),
            RuntimePermissionPromptDecision::Prompt
        );
        assert_eq!(
            RuntimePermissionPolicy::decide_request_permissions_prompt(
                ApprovalMode::AutoEdit,
                false
            ),
            RuntimePermissionPromptDecision::Reject {
                reason: "request_permissions requires a runtime permission handler unless approval mode is full-auto",
            }
        );
    }
}
