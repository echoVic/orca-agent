use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::config::RunConfig;
use orca_core::tool_types::{ToolRequest, ToolResult};

use crate::runtime_permission::TurnPermissionOverlay;

#[derive(Clone, Debug)]
pub enum RuntimeApprovalDecision {
    NotRequired,
    Allowed(ApprovalResolution),
    Ask(ApprovalRequest),
    Denied {
        resolution: ApprovalResolution,
        result: ToolResult,
    },
}

pub trait RuntimeApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution>;
}

pub struct RuntimeConfigApprovalHandler<'a> {
    config: &'a RunConfig,
}

pub(crate) struct RuntimeToolApprovalPolicy<'a> {
    policy: &'a ApprovalPolicy,
    permission_overlay: &'a mut TurnPermissionOverlay,
}

impl<'a> RuntimeToolApprovalPolicy<'a> {
    pub(crate) fn new(
        policy: &'a ApprovalPolicy,
        permission_overlay: &'a mut TurnPermissionOverlay,
    ) -> Self {
        Self {
            policy,
            permission_overlay,
        }
    }

    #[cfg(test)]
    pub(crate) fn resolve(
        &mut self,
        approval: ApprovalRequest,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        self.resolve_with_targets(approval, request, &[], None)
    }

    pub(crate) fn resolve_with_targets(
        &mut self,
        approval: ApprovalRequest,
        request: &ToolRequest,
        targets: &[&str],
        prepared_patch: Option<&orca_tools::apply_patch::PreparedPatch>,
    ) -> RuntimeApprovalDecision {
        let preapproved = self
            .permission_overlay
            .consume_preapproved_tool_call_id(&request.id);
        if preapproved && targets.is_empty() {
            return RuntimeApprovalDecision::Allowed(ApprovalResolution {
                id: approval.id,
                decision: ApprovalDecision::Allow,
                reason: "approved background continuation".to_string(),
            });
        }

        // Rules match what the invocation resolved: the approval carries the
        // normalized tool and the target derived from its arguments, which a
        // streaming provider's raw request lacks.
        let tool = approval
            .tool
            .clone()
            .unwrap_or_else(|| request.name.as_str().to_string());
        let target = approval.target.clone().or_else(|| request.target.clone());
        let decision =
            resolve_for_request(self.policy, &approval, targets, &tool, target.as_deref());
        if preapproved && decision.decision != ApprovalDecision::Deny {
            let approved = self.permission_overlay.preapproved_patch_fingerprint.take();
            if approved.is_some()
                && approved == prepared_patch.and_then(|patch| patch.approval_fingerprint())
            {
                return RuntimeApprovalDecision::Allowed(ApprovalResolution {
                    id: approval.id,
                    decision: ApprovalDecision::Allow,
                    reason: "approved background continuation".to_string(),
                });
            }
            return RuntimeApprovalDecision::Ask(approval);
        }
        if self.permission_overlay.strict_auto_review()
            && decision.decision == ApprovalDecision::Allow
        {
            return RuntimeApprovalDecision::Ask(approval);
        }

        match decision.decision {
            ApprovalDecision::Allow => RuntimeApprovalDecision::Allowed(decision),
            ApprovalDecision::Ask => RuntimeApprovalDecision::Ask(approval),
            ApprovalDecision::Deny => {
                let result = ToolResult::denied(request, decision.reason.clone());
                RuntimeApprovalDecision::Denied {
                    resolution: decision,
                    result,
                }
            }
        }
    }
}

pub(crate) fn resolve_for_request(
    policy: &ApprovalPolicy,
    approval: &ApprovalRequest,
    targets: &[&str],
    tool: &str,
    target: Option<&str>,
) -> ApprovalResolution {
    if !targets.is_empty() {
        return targets
            .iter()
            .map(|path| policy.resolve_for_tool(approval, tool, Some(path)))
            .max_by_key(|resolution| match resolution.decision {
                ApprovalDecision::Deny => 2,
                ApprovalDecision::Ask => 1,
                ApprovalDecision::Allow => 0,
            })
            .unwrap_or_else(|| policy.resolve_for_tool(approval, tool, None));
    }
    policy.resolve_for_tool(approval, tool, target)
}

impl<'a> RuntimeConfigApprovalHandler<'a> {
    pub fn new(config: &'a RunConfig) -> Self {
        Self { config }
    }
}

impl RuntimeApprovalHandler for RuntimeConfigApprovalHandler<'_> {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        crate::approval_resolution::resolve_interactive(self.config, approval, request)
    }
}

#[cfg(test)]
mod tests {
    use orca_approval::ApprovalPolicy;
    use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest};
    use orca_core::tool_types::{ToolName, ToolRequest};

    use super::{RuntimeApprovalDecision, RuntimeToolApprovalPolicy};
    use crate::runtime_permission::TurnPermissionOverlay;

    fn shell_approval_and_request() -> (ApprovalRequest, ToolRequest) {
        (
            ApprovalRequest {
                id: "approval-1".to_string(),
                action: ActionKind::Shell,
                description: "run shell command".to_string(),
                tool: Some("bash".to_string()),
                target: Some("echo hi".to_string()),
                preview: None,
            },
            ToolRequest {
                id: "shell-1".to_string(),
                name: ToolName::Bash,
                action: ActionKind::Shell,
                target: Some("echo hi".to_string()),
                raw_arguments: None,
            },
        )
    }

    #[test]
    fn runtime_tool_approval_policy_preserves_preapproved_allow_under_strict_auto_review() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("shell-1".to_string()));
        overlay.merge_strict_auto_review(true);
        let (approval, request) = shell_approval_and_request();

        let decision =
            RuntimeToolApprovalPolicy::new(&policy, &mut overlay).resolve(approval, &request);

        match decision {
            RuntimeApprovalDecision::Allowed(resolution) => {
                assert_eq!(resolution.decision, ApprovalDecision::Allow);
                assert_eq!(resolution.reason, "approved background continuation");
            }
            other => panic!("preapproved request should be allowed, got {other:?}"),
        }
        assert!(!overlay.consume_preapproved_tool_call_id("shell-1"));
    }

    #[test]
    fn an_approved_patch_with_multiple_targets_can_continue() {
        use orca_core::approval_rules::{PermissionRule, PermissionRules};
        use orca_core::approval_types::Decision;

        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("one.txt"), "before\n").unwrap();
        let (mut approval, mut request) = shell_approval_and_request();
        approval.action = ActionKind::Write;
        approval.tool = Some("apply_patch".into());
        request.action = ActionKind::Write;
        request.name = ToolName::ApplyPatch;
        request.raw_arguments = Some(serde_json::json!({"input": "*** Begin Patch\n*** Delete File: one.txt\n*** Add File: two.txt\n+new\n*** End Patch"}).to_string());
        let approved = orca_tools::apply_patch::prepare(&request, cwd.path(), || false).unwrap();
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let denying =
            ApprovalPolicy::new(ApprovalMode::Suggest).with_permission_rules(PermissionRules {
                rules: vec![PermissionRule::new(
                    "apply_patch",
                    "two.txt",
                    Decision::Deny,
                )],
            });
        std::fs::write(cwd.path().join("one.txt"), "edited\n").unwrap();
        let edited = orca_tools::apply_patch::prepare(&request, cwd.path(), || false).unwrap();
        for (policy, patch, expected) in [
            (&policy, &approved, ApprovalDecision::Allow),
            (&policy, &edited, ApprovalDecision::Ask),
            (&denying, &approved, ApprovalDecision::Deny),
        ] {
            let mut overlay = TurnPermissionOverlay::default();
            overlay.set_preapproved_tool_call_id(Some(request.id.clone()));
            overlay.preapproved_patch_fingerprint = Some(approved.approval_fingerprint().unwrap());
            let targets: Vec<_> = patch.approval_targets().collect();
            let decision = RuntimeToolApprovalPolicy::new(policy, &mut overlay)
                .resolve_with_targets(approval.clone(), &request, &targets, Some(patch));
            let actual = match decision {
                RuntimeApprovalDecision::Allowed(_) => ApprovalDecision::Allow,
                RuntimeApprovalDecision::Ask(_) => ApprovalDecision::Ask,
                RuntimeApprovalDecision::Denied { .. } => ApprovalDecision::Deny,
                other => panic!("unexpected decision {other:?}"),
            };
            assert_eq!(actual, expected);
            assert!(!overlay.consume_preapproved_tool_call_id(&request.id));
        }
    }

    #[test]
    fn a_rule_matches_the_target_the_invocation_derived_when_the_provider_sent_none() {
        // A streaming provider names no target; the invocation derives one
        // from the arguments, and a configured rule must see that one.
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto).with_permission_rules(
            orca_core::approval_rules::PermissionRules {
                rules: vec![orca_core::approval_rules::PermissionRule::new(
                    "write_file",
                    "/etc/**",
                    orca_core::approval_types::Decision::Deny,
                )],
            },
        );
        let mut overlay = TurnPermissionOverlay::default();
        let approval = ApprovalRequest {
            id: "approval-1".to_string(),
            action: ActionKind::Write,
            description: "write_file requested write".to_string(),
            tool: Some("write_file".to_string()),
            target: Some("/etc/hosts".to_string()),
            preview: None,
        };
        let request = ToolRequest {
            id: "write-1".to_string(),
            name: ToolName::WriteFile,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(r#"{"path":"/etc/hosts","content":"x"}"#.to_string()),
        };

        let decision =
            RuntimeToolApprovalPolicy::new(&policy, &mut overlay).resolve(approval, &request);

        assert!(
            matches!(decision, RuntimeApprovalDecision::Denied { .. }),
            "the deny rule must apply, got {decision:?}"
        );
    }

    #[test]
    fn runtime_tool_approval_policy_downgrades_auto_allow_when_strict_auto_review_is_enabled() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.merge_strict_auto_review(true);
        let (approval, request) = shell_approval_and_request();

        let decision =
            RuntimeToolApprovalPolicy::new(&policy, &mut overlay).resolve(approval, &request);

        assert!(matches!(decision, RuntimeApprovalDecision::Ask(_)));
    }
}
