#[cfg(test)]
use orca_core::approval_rules::PermissionRule;
use orca_core::approval_rules::{CompiledPermissionRules, PermissionRules};
use orca_core::approval_types::{
    ActionKind, ApprovalBehavior, ApprovalDecision, ApprovalMode, ApprovalRequest,
    ApprovalResolution, Decision,
};

#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    mode: ApprovalMode,
    rules: CompiledPermissionRules,
}

impl ApprovalPolicy {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            rules: CompiledPermissionRules::default(),
        }
    }

    #[cfg(test)]
    pub fn with_rules(mut self, rules: Vec<PermissionRule>) -> Self {
        self.rules = CompiledPermissionRules::from_rules(PermissionRules { rules });
        self
    }

    pub fn with_permission_rules(mut self, rules: PermissionRules) -> Self {
        self.rules = CompiledPermissionRules::from_rules(rules);
        self
    }

    pub fn behavior(&self) -> ApprovalBehavior {
        self.mode.behavior()
    }

    #[cfg(test)]
    pub fn resolve(&self, request: &ApprovalRequest) -> ApprovalResolution {
        self.resolve_for_tool(request, "", None)
    }

    pub fn resolve_for_tool(
        &self,
        request: &ApprovalRequest,
        tool: &str,
        target: Option<&str>,
    ) -> ApprovalResolution {
        // Plan is a hard capability ceiling. Configuration rules can tighten
        // it further, but they can never mint write, network, agent, or shell
        // authority.
        if self.mode == ApprovalMode::Plan
            && matches!(
                request.action,
                ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell
            )
        {
            return ApprovalResolution {
                id: request.id.clone(),
                decision: ApprovalDecision::Deny,
                reason: format!("plan denies {}", request.action.as_str()),
            };
        }

        if let Some(decision) = self.rules.matching_decision(tool, target) {
            let approval_decision = match decision {
                Decision::Allow => ApprovalDecision::Allow,
                Decision::Prompt => ApprovalDecision::Ask,
                Decision::Deny => ApprovalDecision::Deny,
            };
            return ApprovalResolution {
                id: request.id.clone(),
                decision: approval_decision,
                reason: format!(
                    "permission {} rule matches {tool} {}",
                    decision.as_str(),
                    target.unwrap_or("")
                ),
            };
        }

        // Auto-edit means autonomous execution inside the active sandbox. Any
        // request to extend that sandbox is still handled by the runtime
        // permission path rather than this tool approval policy.
        let decision = match (self.mode, request.action) {
            (_, ActionKind::Read) => ApprovalDecision::Allow,
            (
                ApprovalMode::Plan,
                ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Deny,
            (
                ApprovalMode::Suggest,
                ActionKind::Write | ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Ask,
            (ApprovalMode::AutoEdit, ActionKind::Write) => ApprovalDecision::Allow,
            (
                ApprovalMode::AutoEdit,
                ActionKind::Network | ActionKind::Agent | ActionKind::Shell,
            ) => ApprovalDecision::Allow,
            (ApprovalMode::FullAuto, _) => ApprovalDecision::Allow,
        };

        let reason = match decision {
            ApprovalDecision::Allow => {
                format!("{} permits {}", self.mode.as_str(), request.action.as_str())
            }
            ApprovalDecision::Ask => {
                format!(
                    "{} requires confirmation for {}",
                    self.mode.as_str(),
                    request.action.as_str()
                )
            }
            ApprovalDecision::Deny => {
                format!("{} denies {}", self.mode.as_str(), request.action.as_str())
            }
        };

        ApprovalResolution {
            id: request.id.clone(),
            decision,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(action: ActionKind) -> ApprovalRequest {
        ApprovalRequest {
            id: "test-1".to_string(),
            action,
            description: "test".to_string(),
            tool: None,
            target: None,
            preview: None,
        }
    }

    #[test]
    fn suggest_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn suggest_asks_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn suggest_asks_shell() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let res = policy.resolve(&make_request(ActionKind::Shell));
        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn auto_edit_allows_read() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        let res = policy.resolve(&make_request(ActionKind::Read));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn auto_edit_allows_write() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        let res = policy.resolve(&make_request(ActionKind::Write));
        assert_eq!(res.decision, ApprovalDecision::Allow);
    }

    #[test]
    fn auto_edit_allows_sandboxed_actions() {
        let policy = ApprovalPolicy::new(ApprovalMode::AutoEdit);
        for action in [
            ActionKind::Read,
            ActionKind::Write,
            ActionKind::Network,
            ActionKind::Agent,
            ActionKind::Shell,
        ] {
            assert_eq!(
                policy.resolve(&make_request(action)).decision,
                ApprovalDecision::Allow,
                "auto-edit should allow {action:?} inside its sandbox"
            );
        }
    }

    #[test]
    fn full_auto_allows_all() {
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Read)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Write)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Shell)).decision,
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn plan_mode_allows_read_and_denies_mutation() {
        let policy = ApprovalPolicy::new(ApprovalMode::Plan);
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Read)).decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Write)).decision,
            ApprovalDecision::Deny
        );
        assert_eq!(
            policy.resolve(&make_request(ActionKind::Shell)).decision,
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn plan_mode_hard_ceiling_cannot_be_overridden_by_allow_rule() {
        let policy = ApprovalPolicy::new(ApprovalMode::Plan).with_rules(vec![PermissionRule::new(
            "bash",
            "cargo *",
            Decision::Allow,
        )]);
        let request = ApprovalRequest {
            id: "plan-shell".to_string(),
            action: ActionKind::Shell,
            description: "plan must not execute shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("cargo test".to_string()),
            preview: None,
        };

        let resolution = policy.resolve_for_tool(&request, "bash", Some("cargo test"));

        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert!(resolution.reason.contains("plan"));
    }

    #[test]
    fn resolution_preserves_request_id() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let req = ApprovalRequest {
            id: "custom-id-42".to_string(),
            action: ActionKind::Read,
            description: "test".to_string(),
            tool: None,
            target: None,
            preview: None,
        };
        let res = policy.resolve(&req);
        assert_eq!(res.id, "custom-id-42");
    }

    #[test]
    fn matching_deny_rule_overrides_full_auto() {
        let policy =
            ApprovalPolicy::new(ApprovalMode::FullAuto).with_rules(vec![PermissionRule::new(
                "bash",
                "rm -rf *",
                Decision::Deny,
            )]);
        let req = ApprovalRequest {
            id: "danger".to_string(),
            action: ActionKind::Shell,
            description: "bash requested rm -rf target".to_string(),
            tool: Some("bash".to_string()),
            target: Some("rm -rf target".to_string()),
            preview: None,
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("rm -rf target"));

        assert_eq!(res.decision, ApprovalDecision::Deny);
        assert!(res.reason.contains("permission deny rule"));
    }

    #[test]
    fn matching_allow_rule_overrides_suggest_prompt() {
        let policy =
            ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![PermissionRule::new(
                "bash",
                "cargo *",
                Decision::Allow,
            )]);
        let req = ApprovalRequest {
            id: "cargo".to_string(),
            action: ActionKind::Shell,
            description: "bash requested cargo test".to_string(),
            tool: Some("bash".to_string()),
            target: Some("cargo test".to_string()),
            preview: None,
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("cargo test"));

        assert_eq!(res.decision, ApprovalDecision::Allow);
        assert!(res.reason.contains("permission allow rule"));
    }

    #[test]
    fn no_matching_rule_uses_mode_default() {
        let policy =
            ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![PermissionRule::new(
                "bash",
                "cargo *",
                Decision::Allow,
            )]);
        let req = ApprovalRequest {
            id: "other".to_string(),
            action: ActionKind::Shell,
            description: "bash requested npm test".to_string(),
            tool: Some("bash".to_string()),
            target: Some("npm test".to_string()),
            preview: None,
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("npm test"));

        assert_eq!(res.decision, ApprovalDecision::Ask);
    }

    #[test]
    fn prompt_rule_overrides_full_auto_to_ask() {
        let policy =
            ApprovalPolicy::new(ApprovalMode::FullAuto).with_rules(vec![PermissionRule::new(
                "bash",
                "curl *",
                Decision::Prompt,
            )]);
        let req = ApprovalRequest {
            id: "curl".to_string(),
            action: ActionKind::Shell,
            description: "bash requested curl example.com".to_string(),
            tool: Some("bash".to_string()),
            target: Some("curl example.com".to_string()),
            preview: None,
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("curl example.com"));

        assert_eq!(res.decision, ApprovalDecision::Ask);
        assert!(res.reason.contains("permission prompt rule"));
    }

    #[test]
    fn strictest_matching_rule_wins() {
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest).with_rules(vec![
            PermissionRule::new("bash", "cargo *", Decision::Allow),
            PermissionRule::new("bash", "cargo publish *", Decision::Prompt),
            PermissionRule::new("bash", "cargo publish secret*", Decision::Deny),
        ]);
        let req = ApprovalRequest {
            id: "publish".to_string(),
            action: ActionKind::Shell,
            description: "bash requested cargo publish secret-crate".to_string(),
            tool: Some("bash".to_string()),
            target: Some("cargo publish secret-crate".to_string()),
            preview: None,
        };

        let res = policy.resolve_for_tool(&req, "bash", Some("cargo publish secret-crate"));

        assert_eq!(res.decision, ApprovalDecision::Deny);
        assert!(res.reason.contains("permission deny rule"));
    }
}
