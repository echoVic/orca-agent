use std::path::Path;

use orca_core::approval_types::ApprovalMode;
use orca_core::capability::{EnforcementState, SandboxEnforcementDecision};
use orca_core::config::RunConfig;
use orca_core::tool_types::ToolName;
use orca_tools::sandbox::{
    ReadOnlySandboxCommandContext, SandboxPolicyRefusal, WorkspaceWriteSandboxCommandContext,
};

use crate::server::CommandExecSandbox;
use crate::shell_session::ShellSandboxMode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellReadiness {
    Available,
    Blocked { detail: String, remediation: String },
}

impl ShellReadiness {
    pub(crate) fn for_config(config: &RunConfig) -> Self {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let sandbox = match crate::server::bash_sandbox_for_cwd(config, &cwd) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                return Self::Blocked {
                    detail: format!("shell sandbox policy could not be resolved: {error}"),
                    remediation:
                        "repair the active permission profile before starting shell commands"
                            .to_string(),
                };
            }
        };
        let decision = config
            .tools
            .shell_enforcement_decision
            .clone()
            .unwrap_or_else(orca_tools::sandbox::enforcement_decision);
        let refusal = shell_policy_refusal(config, &cwd, &sandbox, &decision);
        Self::from_sandbox_mode(sandbox.mode, decision, refusal, cfg!(windows))
    }

    pub(crate) fn for_surface_settings(
        base_config: &RunConfig,
        settings: &crate::surface::SurfaceRuntimeSettings,
    ) -> Self {
        let mut config = base_config.clone();
        config.approval_mode = match settings.approval_mode {
            crate::surface::SurfaceApprovalMode::Suggest => ApprovalMode::Suggest,
            crate::surface::SurfaceApprovalMode::AutoEdit => ApprovalMode::AutoEdit,
            crate::surface::SurfaceApprovalMode::FullAuto => ApprovalMode::FullAuto,
            crate::surface::SurfaceApprovalMode::Plan => ApprovalMode::Plan,
        };
        config.cwd = Some(settings.cwd.as_path().to_path_buf());
        config.runtime_workspace_roots = Some(
            settings
                .workspace_roots
                .iter()
                .map(|root| root.as_path().to_path_buf())
                .collect(),
        );
        config.active_permission_profile =
            settings.active_permission_profile.as_ref().map(|profile| {
                orca_core::config::ActivePermissionProfile {
                    id: profile.id.as_str().to_string(),
                    extends: profile
                        .extends
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                }
            });
        Self::for_config(&config)
    }

    fn from_sandbox_mode(
        mode: ShellSandboxMode,
        decision: SandboxEnforcementDecision,
        refusal: Option<SandboxPolicyRefusal>,
        native_windows: bool,
    ) -> Self {
        if mode == ShellSandboxMode::DangerFullAccess || native_windows {
            return Self::Available;
        }
        if decision.state == EnforcementState::Enforced {
            return match refusal {
                None => Self::Available,
                Some(refusal) => Self::Blocked {
                    detail: refusal.detail,
                    remediation: refusal.remediation,
                },
            };
        }
        Self::Blocked {
            detail: orca_tools::sandbox::enforcement_unavailable_message(
                &decision.backend,
                &decision.probes,
            ),
            remediation: orca_tools::sandbox::enforcement_unavailable_remediation(&decision.probes),
        }
    }

    pub(crate) fn blocks_new_processes(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub(crate) fn blocks_tool(&self, tool: &ToolName) -> bool {
        self.blocks_new_processes() && matches!(tool, ToolName::Bash)
    }

    pub(crate) fn blocks_tool_name(&self, tool: &str) -> bool {
        self.blocks_new_processes() && tool == "bash"
    }

    pub(crate) fn failure_message(&self) -> Option<String> {
        let Self::Blocked {
            detail,
            remediation,
        } = self
        else {
            return None;
        };
        // Probe evidence quotes the backend's own message, which may end
        // with a full stop already.
        Some(format!("{}. {remediation}", detail.trim_end_matches('.')))
    }

    /// Warn when the workspace has no trust decision and the effective default profile is
    /// therefore read-only. Without this the only symptom is the shell's own
    /// `Read-only file system`, which reads like a permission bug rather than a missing trust
    /// decision (issue #73).
    /// Every start-up warning that can be derived from the run config, in the order they are
    /// shown. Used both for the session handle and for the headless `session.started` payload,
    /// so the stream and the terminal agree.
    pub(crate) fn run_startup_warnings(config: &RunConfig) -> Vec<String> {
        Self::for_config(config)
            .startup_warning()
            .into_iter()
            .chain(Self::untrusted_workspace_warning(config))
            .collect()
    }

    pub(crate) fn untrusted_workspace_warning(config: &RunConfig) -> Option<String> {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if orca_core::config::folder_trust::is_trusted(&cwd) {
            return None;
        }
        let sandbox = crate::server::bash_sandbox_for_cwd(config, &cwd).ok()?;
        if !matches!(sandbox.mode, ShellSandboxMode::ReadOnly { .. }) {
            return None;
        }
        Some(format!(
            "Workspace {} is untrusted: shell commands run sandboxed read-only, so writes fail \
             with `Read-only file system`. Run `orca trust add --cwd {}` (after reviewing the \
             folder) to allow changes, or select a trusted-host permission profile.",
            cwd.display(),
            cwd.display()
        ))
    }

    pub(crate) fn startup_warning(&self) -> Option<String> {
        self.failure_message().map(|reason| {
            format!(
                "Shell unavailable under the current restricted policy: {reason}. Dedicated file \
                 tools remain available. \
                 Repair the host sandbox or explicitly select a trusted-host policy if direct host \
                 execution is intended; full-auto does so only when no stricter permission profile \
                 is active."
            )
        })
    }

    pub(crate) fn model_context(&self, approval_mode: ApprovalMode) -> Option<String> {
        self.failure_message().map(|reason| {
            format!(
                "## Shell availability\n\
                 The `bash` command entry point is unavailable in {} mode and has been removed \
                 from the tool catalog. Do not call it. Continue with the dedicated file and \
                 search tools that remain available. Reason: {reason}. Do not use `/trust` as a \
                 workaround; only the user may explicitly select a trusted-host policy.",
                approval_mode.as_str()
            )
        })
    }
}

/// Why the backend that enforces here would still refuse the shell's
/// commands. Asked of the policy a command starts from, as the bash tool
/// builds it, with the launch's own question: a shell reported available is
/// one whose commands run (issue #118).
fn shell_policy_refusal(
    config: &RunConfig,
    cwd: &Path,
    sandbox: &CommandExecSandbox,
    decision: &SandboxEnforcementDecision,
) -> Option<SandboxPolicyRefusal> {
    let mut additional_roots = config
        .additional_working_directories
        .iter()
        .filter(|directory| {
            directory.source != crate::runtime_permission::SESSION_METADATA_DIRECTORY_SOURCE
        })
        .map(|directory| directory.path.clone())
        .collect::<Vec<_>>();
    for root in &sandbox.additional_writable_roots {
        if !additional_roots.contains(root) {
            additional_roots.push(root.clone());
        }
    }
    match sandbox.mode {
        ShellSandboxMode::WorkspaceWrite {
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => orca_tools::sandbox::workspace_write_policy_refusal(
            &WorkspaceWriteSandboxCommandContext {
                command: "true",
                cwd,
                readable_roots: &sandbox.additional_readable_roots,
                additional_roots: &additional_roots,
                metadata_writable_roots: &sandbox.metadata_writable_roots,
                metadata_read_only_paths: &[],
                denied_roots: &sandbox.denied_writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                allowed_unix_socket_roots: &sandbox.allowed_unix_socket_roots,
            },
            decision,
        ),
        ShellSandboxMode::ReadOnly {
            network_access,
            allow_global_read,
        } => orca_tools::sandbox::read_only_policy_refusal(
            &ReadOnlySandboxCommandContext {
                command: "true",
                cwd,
                readable_roots: &sandbox.additional_readable_roots,
                additional_roots: &additional_roots,
                metadata_writable_roots: &sandbox.metadata_writable_roots,
                denied_roots: &sandbox.denied_writable_roots,
                network_access,
                allow_global_read,
                allowed_unix_socket_roots: &sandbox.allowed_unix_socket_roots,
            },
            decision,
        ),
        ShellSandboxMode::DangerFullAccess => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::capability::{SandboxProbeEvidence, SandboxProbeStatus};

    fn unavailable_decision() -> SandboxEnforcementDecision {
        SandboxEnforcementDecision::new(
            EnforcementState::Unavailable,
            "seatbelt",
            vec![SandboxProbeEvidence {
                backend: "seatbelt".to_string(),
                executable: Some("/usr/bin/sandbox-exec".into()),
                status: SandboxProbeStatus::ProbeDenied,
                exit_code: None,
                signal: Some(6),
                stderr: None,
                io_error: None,
            }],
        )
    }

    #[test]
    fn restricted_mode_blocks_only_new_shell_process_tools() {
        let readiness = ShellReadiness::from_sandbox_mode(
            ShellSandboxMode::WorkspaceWrite {
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            unavailable_decision(),
            None,
            false,
        );

        assert!(readiness.blocks_tool(&ToolName::Bash));
        assert!(
            !readiness.blocks_tool(&ToolName::TaskReadOutput),
            "reading an existing task's output starts no process"
        );
        assert!(!readiness.blocks_tool(&ToolName::TaskWait));
        assert!(!readiness.blocks_tool(&ToolName::Edit));
    }

    #[test]
    fn blocked_messages_preserve_evidence_and_recovery_boundary() {
        let readiness = ShellReadiness::from_sandbox_mode(
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: true,
            },
            unavailable_decision(),
            None,
            false,
        );

        let warning = readiness.startup_warning().expect("blocked warning");
        assert!(warning.contains("seatbelt"));
        assert!(warning.contains("signal 6"));
        assert!(warning.contains("Dedicated file tools remain available"));
        assert!(warning.contains("explicitly select a trusted-host policy"));

        let context = readiness
            .model_context(ApprovalMode::AutoEdit)
            .expect("blocked context");
        assert!(context.contains("removed from the tool catalog"));
        assert!(context.contains("Do not use `/trust` as a workaround"));
        assert!(context.contains("trusted-host policy"));
    }

    #[test]
    fn trusted_host_mode_remains_available_without_os_enforcement() {
        let readiness = ShellReadiness::from_sandbox_mode(
            ShellSandboxMode::DangerFullAccess,
            unavailable_decision(),
            None,
            false,
        );

        assert_eq!(readiness, ShellReadiness::Available);
    }

    fn landlock_only_decision() -> SandboxEnforcementDecision {
        SandboxEnforcementDecision::new(
            EnforcementState::Enforced,
            "landlock+seccomp",
            vec![SandboxProbeEvidence {
                backend: "bwrap".to_string(),
                executable: Some("/usr/bin/bwrap".into()),
                status: SandboxProbeStatus::ProbeDenied,
                exit_code: Some(1),
                signal: None,
                stderr: Some(
                    "bwrap: Can't mount proc on /newroot/proc: Operation not permitted".to_string(),
                ),
                io_error: None,
            }],
        )
    }

    #[test]
    fn an_enforcing_backend_that_refuses_the_policy_blocks_the_shell_with_its_reason() {
        let refusal = SandboxPolicyRefusal {
            detail: "this shell policy keeps /work/.git read-only inside a writable root"
                .to_string(),
            remediation: "let bubblewrap create its namespaces on this host".to_string(),
        };
        let readiness = ShellReadiness::from_sandbox_mode(
            ShellSandboxMode::WorkspaceWrite {
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            landlock_only_decision(),
            Some(refusal.clone()),
            false,
        );

        assert!(readiness.blocks_tool(&ToolName::Bash));
        let warning = readiness.startup_warning().expect("blocked warning");
        assert!(warning.contains(&refusal.detail), "{warning}");
        assert!(warning.contains(&refusal.remediation), "{warning}");
    }

    #[test]
    fn an_enforcing_backend_that_accepts_the_policy_leaves_the_shell_available() {
        let readiness = ShellReadiness::from_sandbox_mode(
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            },
            landlock_only_decision(),
            None,
            false,
        );

        assert_eq!(readiness, ShellReadiness::Available);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_landlock_only_host_blocks_the_shell_of_a_trusted_workspace() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join(".git")).unwrap();
        orca_core::config::folder_trust::set_trust_with_config_dir(
            cwd.path(),
            home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
        let config = RunConfig {
            cwd: Some(cwd.path().to_path_buf()),
            approval_mode: ApprovalMode::AutoEdit,
            tools: orca_core::config::ToolConfig {
                shell_enforcement_decision: Some(landlock_only_decision()),
                ..orca_core::config::ToolConfig::default()
            },
            ..RunConfig::default()
        };

        let message = ShellReadiness::for_config(&config)
            .failure_message()
            .expect("the trusted workspace's shell policy needs bubblewrap");

        assert!(message.contains("bubblewrap"), "{message}");
        assert!(message.contains("Can't mount proc"), "{message}");
    }
}
