use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::PermissionProfileNetworkAccess;

use crate::network_proxy::RuntimeNetworkBlockReport;
use crate::protocol::{
    PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
    RequestNetworkPermissions, RequestPermissionProfile, RequestShellPermissions,
};
use crate::sandbox_denial::{
    SandboxDenialDiagnostic, should_request_filesystem_permission_with_denied_roots,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionRequest {
    pub id: String,
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
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
    FilesystemWrite,
    UnsandboxedShellRetry,
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
                    shell: None,
                },
            },
        })
    }

    pub fn filesystem_write_decision(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionDecision> {
        let write_root = diagnostic.suggested_write_root.as_ref()?.clone();
        Some(RuntimePermissionDecision {
            origin,
            kind: RuntimePermissionRequestKind::FilesystemWrite,
            request: RuntimePermissionRequest {
                id: request_id.to_string(),
                reason: Some(format!(
                    "{} attempted filesystem write outside the current sandbox: {}",
                    origin.label(),
                    write_root.display()
                )),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![write_root]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
            },
        })
    }

    pub fn unsandboxed_shell_decision(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionDecision> {
        if diagnostic.suggested_write_root.is_some() {
            return None;
        }

        Some(RuntimePermissionDecision {
            origin,
            kind: RuntimePermissionRequestKind::UnsandboxedShellRetry,
            request: RuntimePermissionRequest {
                id: request_id.to_string(),
                reason: Some(format!(
                    "{} needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant",
                    origin.label()
                )),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: None,
                    shell: Some(RequestShellPermissions { unsandboxed: true }),
                },
            },
        })
    }

    pub fn sandbox_denial_decision(
        request_id: &str,
        origin: RuntimePermissionOrigin,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> RuntimePermissionDecision {
        Self::filesystem_write_decision(request_id, origin, diagnostic).unwrap_or_else(|| {
            Self::unsandboxed_shell_decision(request_id, origin, diagnostic)
                .expect("pathless sandbox denial should request unsandboxed shell retry")
        })
    }

    pub(crate) fn should_request_filesystem_retry(
        cwd: &std::path::Path,
        diagnostic: &SandboxDenialDiagnostic,
        denied_writable_roots: &[PathBuf],
    ) -> bool {
        should_request_filesystem_permission_with_denied_roots(
            cwd,
            diagnostic,
            denied_writable_roots,
        ) || diagnostic.suggested_write_root.is_none()
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
                if is_exact_metadata_root(root) {
                    if !self.metadata_writable_directories.contains(root) {
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

fn is_exact_metadata_root(path: &std::path::Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".agents" | ".codex")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::PermissionProfileNetworkAccess;

    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::protocol::{RequestFileSystemPermissions, RequestPermissionProfile};
    use crate::sandbox_denial::SandboxDenialDiagnostic;

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
    fn runtime_permission_policy_builds_sandbox_denial_requests() {
        let write_diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };
        let pathless_diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        let write_decision = RuntimePermissionPolicy::sandbox_denial_decision(
            "permission-1",
            RuntimePermissionOrigin::Bash,
            &write_diagnostic,
        );
        let unsandboxed_decision = RuntimePermissionPolicy::sandbox_denial_decision(
            "permission-2",
            RuntimePermissionOrigin::CommandExec,
            &pathless_diagnostic,
        );

        assert_eq!(write_decision.origin, RuntimePermissionOrigin::Bash);
        assert_eq!(
            write_decision.kind,
            RuntimePermissionRequestKind::FilesystemWrite
        );
        assert_eq!(
            write_decision
                .request
                .permissions
                .file_system
                .as_ref()
                .and_then(|file_system| file_system.write.as_ref()),
            Some(&vec![PathBuf::from("/repo/.git")])
        );
        assert_eq!(
            write_decision.request.reason.as_deref(),
            Some("bash attempted filesystem write outside the current sandbox: /repo/.git")
        );
        assert_eq!(
            unsandboxed_decision.origin,
            RuntimePermissionOrigin::CommandExec
        );
        assert_eq!(
            unsandboxed_decision.kind,
            RuntimePermissionRequestKind::UnsandboxedShellRetry
        );
        assert_eq!(
            unsandboxed_decision
                .request
                .permissions
                .shell
                .as_ref()
                .map(|shell| shell.unsandboxed),
            Some(true)
        );
        assert_eq!(
            unsandboxed_decision.request.reason.as_deref(),
            Some(
                "command/exec needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant"
            )
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
