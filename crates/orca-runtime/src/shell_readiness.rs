use orca_core::approval_types::ApprovalMode;
use orca_core::capability::{EnforcementState, SandboxEnforcementDecision};
use orca_core::config::RunConfig;
use orca_core::tool_types::ToolName;

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
        Self::from_sandbox_mode(
            sandbox.mode,
            config
                .tools
                .shell_enforcement_decision
                .clone()
                .unwrap_or_else(orca_tools::sandbox::enforcement_decision),
            cfg!(windows),
        )
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
        native_windows: bool,
    ) -> Self {
        if mode == ShellSandboxMode::DangerFullAccess || native_windows {
            return Self::Available;
        }
        if decision.state == EnforcementState::Enforced {
            return Self::Available;
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
        self.blocks_new_processes() && matches!(tool, ToolName::Bash | ToolName::ExecCommand)
    }

    pub(crate) fn blocks_tool_name(&self, tool: &str) -> bool {
        self.blocks_new_processes() && matches!(tool, "bash" | "exec_command")
    }

    pub(crate) fn failure_message(&self) -> Option<String> {
        let Self::Blocked {
            detail,
            remediation,
        } = self
        else {
            return None;
        };
        Some(format!("{detail}. {remediation}"))
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
                 New shell process tools (`bash` and `exec_command`) are unavailable in {} mode \
                 and have been removed from the tool catalog. Do not call them. Continue with the \
                 dedicated file and search tools that remain available. `write_stdin` may only be \
                 used for a terminal session that already exists. Reason: {reason}. Do not use \
                 `/trust` as a workaround; only the user may explicitly select a trusted-host \
                 policy.",
                approval_mode.as_str()
            )
        })
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
            false,
        );

        assert!(readiness.blocks_tool(&ToolName::Bash));
        assert!(readiness.blocks_tool(&ToolName::ExecCommand));
        assert!(!readiness.blocks_tool(&ToolName::WriteStdin));
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
            false,
        );

        assert_eq!(readiness, ShellReadiness::Available);
    }
}
