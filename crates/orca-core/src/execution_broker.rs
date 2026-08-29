use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};

use crate::capability::{
    CapabilityCeiling, CapabilityProcessClass, CapabilityReceipt, EffectiveCapability,
    EnforcementState, SandboxDenialReceipt, SandboxDenialSource,
};
use orca_platform::process::ProcessJob;

#[derive(Debug)]
pub enum LaunchError {
    EnforcementUnavailable,
    EnforcementAdvisory,
    UntrustedProcessClass,
    CapabilityCeilingExceeded,
    NetworkTargetsUnsupported,
    Cwd(io::Error),
    Spawn(io::Error),
}

#[derive(Debug)]
pub struct BrokerLaunch {
    pub child: Child,
    pub process_job: ProcessJob,
    pub receipt: CapabilityReceipt,
}

/// Single process-launch choke point shared by runtime, MCP and integrations.
#[derive(Clone, Debug)]
pub struct ExecutionBroker {
    enforcement: EnforcementState,
    backend: String,
    ceiling: CapabilityCeiling,
}

impl ExecutionBroker {
    pub fn new(enforcement: EnforcementState) -> Self {
        Self::with_backend(enforcement, "runtime-broker")
    }

    pub fn with_backend(enforcement: EnforcementState, backend: impl Into<String>) -> Self {
        Self {
            enforcement,
            backend: backend.into(),
            ceiling: CapabilityCeiling::from(crate::capability::CapabilitySet::all()),
        }
    }

    pub fn with_backend_and_ceiling(
        enforcement: EnforcementState,
        backend: impl Into<String>,
        ceiling: CapabilityCeiling,
    ) -> Self {
        Self {
            enforcement,
            backend: backend.into(),
            ceiling,
        }
    }

    pub fn enforcement(&self) -> EnforcementState {
        self.enforcement
    }

    pub fn launch(
        &self,
        command: Command,
        capability: EffectiveCapability,
    ) -> Result<BrokerLaunch, LaunchError> {
        self.launch_inner(command, capability, None, false)
    }

    pub fn launch_named(
        &self,
        command: Command,
        capability: EffectiveCapability,
        job_name: &str,
    ) -> Result<BrokerLaunch, LaunchError> {
        self.launch_inner(command, capability, Some(job_name), false)
    }

    /// Authorize a platform-native launcher that cannot return a standard
    /// `std::process::Child` (for example Windows AppContainer/ConPTY). The
    /// adapter remains responsible for the kernel operation, while the broker
    /// owns the same class, target and enforcement checks as normal launches.
    pub fn authorize_platform(
        &self,
        capability: &EffectiveCapability,
    ) -> Result<CapabilityReceipt, LaunchError> {
        self.validate_capability(capability, false)?;
        Ok(capability.receipt(self.enforcement, self.backend.clone()))
    }

    /// Construct authority-bearing denial evidence for a backend refusal. The
    /// caller must use this only for an error returned by this broker/backend;
    /// child stdout/stderr is intentionally not accepted here.
    pub fn denial_receipt(
        &self,
        capability: &EffectiveCapability,
        operation: impl Into<String>,
        denied_path: Option<PathBuf>,
    ) -> SandboxDenialReceipt {
        SandboxDenialReceipt::new(
            capability.request_id.clone(),
            SandboxDenialSource::Backend,
            self.backend.clone(),
            denied_path,
            operation,
        )
    }

    #[allow(deprecated)]
    fn launch_inner(
        &self,
        mut command: Command,
        capability: EffectiveCapability,
        job_name: Option<&str>,
        allow_user_trusted: bool,
    ) -> Result<BrokerLaunch, LaunchError> {
        self.validate_capability(&capability, allow_user_trusted)?;

        // The receipt's cwd is the launch boundary, not descriptive metadata.
        // Attach an open directory identity so a path replacement cannot
        // redirect the child between capability resolution and exec.
        crate::workspace_identity::attach_stable_cwd(&mut command, &capability.cwd)
            .map_err(LaunchError::Cwd)?;

        let (child, process_job) = match job_name {
            Some(job_name) => ProcessJob::spawn_named(&mut command, job_name),
            None => ProcessJob::spawn(&mut command),
        }
        .map_err(LaunchError::Spawn)?;
        let receipt = capability.receipt(self.enforcement, self.backend.clone());
        Ok(BrokerLaunch {
            child,
            process_job,
            receipt,
        })
    }

    fn validate_capability(
        &self,
        capability: &EffectiveCapability,
        allow_user_trusted: bool,
    ) -> Result<(), LaunchError> {
        if capability.process_class == CapabilityProcessClass::UserTrustedIntegration
            && !allow_user_trusted
        {
            return Err(LaunchError::UntrustedProcessClass);
        }
        if !allow_user_trusted && !capability.network_targets.is_empty() {
            // The current platform adapters enforce network as an on/off
            // capability. A target list without a target-aware backend would
            // be an audit-only claim, so reject it instead of widening to all
            // network destinations.
            return Err(LaunchError::NetworkTargetsUnsupported);
        }
        capability
            .ensure_subset_of(&self.ceiling)
            .map_err(|_| LaunchError::CapabilityCeilingExceeded)?;
        match self.enforcement {
            EnforcementState::Enforced => {}
            EnforcementState::Unavailable => {
                if capability.process_class != CapabilityProcessClass::UserTrustedIntegration {
                    return Err(LaunchError::EnforcementUnavailable);
                }
            }
            EnforcementState::Advisory => {
                if capability.process_class != CapabilityProcessClass::UserTrustedIntegration {
                    return Err(LaunchError::EnforcementAdvisory);
                }
            }
        }
        Ok(())
    }

    /// Launch an explicitly user-owned integration. This path is intentionally
    /// advisory: it records the requested capability and keeps environment
    /// handling centralized, but never claims kernel sandbox enforcement.
    pub fn launch_user_trusted(
        &self,
        command: Command,
        request_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        capabilities: crate::capability::CapabilitySet,
    ) -> Result<BrokerLaunch, LaunchError> {
        self.launch_user_trusted_inner(command, request_id, cwd, capabilities, None)
    }

    pub fn launch_user_trusted_named(
        &self,
        command: Command,
        request_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        capabilities: crate::capability::CapabilitySet,
        job_name: &str,
    ) -> Result<BrokerLaunch, LaunchError> {
        self.launch_user_trusted_inner(command, request_id, cwd, capabilities, Some(job_name))
    }

    fn launch_user_trusted_inner(
        &self,
        command: Command,
        request_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        capabilities: crate::capability::CapabilitySet,
        job_name: Option<&str>,
    ) -> Result<BrokerLaunch, LaunchError> {
        let request = crate::capability::CapabilityRequest::new(
            request_id,
            CapabilityProcessClass::UserTrustedIntegration,
            capabilities.clone(),
            cwd,
        );
        let effective = EffectiveCapability::resolve_user_trusted(
            request,
            capabilities,
            crate::approval_types::ApprovalMode::FullAuto,
        )
        .map_err(|_| LaunchError::EnforcementAdvisory)?;
        self.launch_inner(command, effective, job_name, true)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{ExecutionBroker, LaunchError};
    use crate::approval_types::ApprovalMode;
    use crate::capability::{
        CapabilityCeiling, CapabilityProcessClass, CapabilityRequest, CapabilitySet,
        EffectiveCapability, EnforcementState, SandboxDenialSource,
    };

    fn read_only_capability(id: &str) -> EffectiveCapability {
        EffectiveCapability::resolve(
            CapabilityRequest::new(
                id,
                CapabilityProcessClass::SandboxedTool,
                CapabilitySet::read_only(),
                std::env::current_dir().expect("current directory"),
            ),
            CapabilitySet::read_only(),
            ApprovalMode::Plan,
        )
        .expect("resolve capability")
    }

    #[test]
    fn unavailable_backend_rejects_non_dangerous_launch() {
        let broker = ExecutionBroker::new(EnforcementState::Unavailable);
        let error = broker
            .launch(Command::new("true"), read_only_capability("broker-1"))
            .expect_err("unavailable backend must reject launch");
        assert!(matches!(error, LaunchError::EnforcementUnavailable));
    }

    #[test]
    fn enforced_launch_returns_receipt() {
        let broker = ExecutionBroker::new(EnforcementState::Enforced);
        let launched = broker
            .launch(Command::new("true"), read_only_capability("broker-2"))
            .expect("launch through broker");
        assert_eq!(launched.receipt.enforcement, EnforcementState::Enforced);
        assert_eq!(launched.receipt.request_id, "broker-2");
    }

    #[test]
    fn ordinary_launch_rejects_a_deserialized_trusted_capability() {
        let capability = EffectiveCapability::resolve_user_trusted(
            CapabilityRequest::new(
                "broker-trusted",
                CapabilityProcessClass::UserTrustedIntegration,
                CapabilitySet::all(),
                std::env::current_dir().expect("current directory"),
            ),
            CapabilitySet::all(),
            ApprovalMode::FullAuto,
        )
        .expect("resolve trusted capability");
        let encoded = serde_json::to_string(&capability).expect("serialize capability");
        let forged: EffectiveCapability =
            serde_json::from_str(&encoded).expect("deserialize capability");
        let broker = ExecutionBroker::new(EnforcementState::Enforced);

        let error = broker
            .launch(Command::new("true"), forged)
            .expect_err("ordinary launch must not accept trusted class");
        assert!(matches!(error, LaunchError::UntrustedProcessClass));
    }

    #[test]
    fn explicit_user_trusted_launcher_accepts_the_trusted_class() {
        let cwd = tempfile::tempdir().expect("cwd");
        let broker = ExecutionBroker::new(EnforcementState::Advisory);
        let launched = broker
            .launch_user_trusted(
                Command::new("true"),
                "broker-explicit-trusted",
                cwd.path().to_path_buf(),
                CapabilitySet::read_only(),
            )
            .expect("explicit trusted launcher");
        assert_eq!(
            launched.receipt.process_class,
            CapabilityProcessClass::UserTrustedIntegration
        );
        let mut child = launched.child;
        let _process_job = launched.process_job;
        assert!(child.wait().expect("wait child").success());
    }

    #[test]
    fn target_scoped_network_is_rejected_until_backend_supports_it() {
        let mut request = CapabilityRequest::new(
            "broker-targets",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::all(),
            std::env::current_dir().expect("current directory"),
        );
        request
            .network_targets
            .insert("api.example.com".to_string());
        let capability =
            EffectiveCapability::resolve(request, CapabilitySet::all(), ApprovalMode::FullAuto)
                .expect("resolve capability");
        let error = ExecutionBroker::new(EnforcementState::Enforced)
            .launch(Command::new("true"), capability)
            .expect_err("target-scoped network needs a target-aware adapter");
        assert!(matches!(error, LaunchError::NetworkTargetsUnsupported));
    }

    #[test]
    fn broker_rejects_a_capability_outside_its_root_ceiling() {
        let cwd = std::env::current_dir().expect("current directory");
        let mut request = CapabilityRequest::new(
            "broker-ceiling",
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet::workspace_write(),
            &cwd,
        );
        request.write_roots = vec![cwd.join("src")];
        let capability = EffectiveCapability::resolve(
            request,
            CapabilitySet::workspace_write(),
            ApprovalMode::AutoEdit,
        )
        .expect("resolve capability");
        let broker = ExecutionBroker::with_backend_and_ceiling(
            EnforcementState::Enforced,
            "test",
            CapabilityCeiling {
                capabilities: CapabilitySet::workspace_write(),
                read_roots: None,
                write_roots: Some(vec![cwd.join("other")]),
                metadata_roots: None,
                denied_roots: Vec::new(),
                network_targets: None,
            },
        );
        let error = broker
            .launch(Command::new("true"), capability)
            .expect_err("root outside ceiling must be rejected");
        assert!(matches!(error, LaunchError::CapabilityCeilingExceeded));
    }

    #[test]
    fn backend_denial_receipt_is_bound_to_broker_backend() {
        let capability = read_only_capability("broker-denial");
        let receipt = ExecutionBroker::with_backend(EnforcementState::Enforced, "landlock+seccomp")
            .denial_receipt(
                &capability,
                "write",
                Some(capability.cwd.join(".git/index.lock")),
            );
        assert_eq!(receipt.source, SandboxDenialSource::Backend);
        assert_eq!(receipt.backend, "landlock+seccomp");
        assert_eq!(receipt.request_id, "broker-denial");
    }

    #[test]
    fn launch_uses_receipt_cwd_instead_of_caller_cwd() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut command = Command::new("true");
        command.current_dir("/path/that/does/not/exist");
        let broker = ExecutionBroker::new(EnforcementState::Enforced);
        let launched = broker
            .launch(
                command,
                EffectiveCapability::resolve(
                    CapabilityRequest::new(
                        "broker-cwd",
                        CapabilityProcessClass::SandboxedTool,
                        CapabilitySet::read_only(),
                        cwd.path().to_path_buf(),
                    ),
                    CapabilitySet::read_only(),
                    ApprovalMode::Plan,
                )
                .expect("resolve capability"),
            )
            .expect("broker must override caller cwd");
        let mut child = launched.child;
        let _process_job = launched.process_job;
        assert!(child.wait().expect("wait child").success());
    }
}
