use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Suggest,
    #[value(name = "auto-edit")]
    #[default]
    AutoEdit,
    FullAuto,
    Plan,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalBehavior {
    Ask,
    Auto,
    Never,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
            Self::Plan => "plan",
        }
    }

    /// Next mode in the Shift+Tab cycle: suggest -> auto-edit -> full-auto -> plan -> suggest.
    pub fn next(self) -> Self {
        match self {
            Self::Suggest => Self::AutoEdit,
            Self::AutoEdit => Self::FullAuto,
            Self::FullAuto => Self::Plan,
            Self::Plan => Self::Suggest,
        }
    }

    pub fn behavior(self) -> ApprovalBehavior {
        match self {
            Self::Suggest => ApprovalBehavior::Ask,
            Self::AutoEdit | Self::Plan => ApprovalBehavior::Auto,
            Self::FullAuto => ApprovalBehavior::Never,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApprovalBehavior, ApprovalMode};

    #[test]
    fn defaults_to_auto_edit_without_changing_the_mode_cycle() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::AutoEdit);
        assert_eq!(ApprovalMode::Suggest.next(), ApprovalMode::AutoEdit);
        assert_eq!(ApprovalMode::Plan.next(), ApprovalMode::Suggest);
    }

    #[test]
    fn full_auto_disables_prompts_without_selecting_trusted_host() {
        assert_eq!(ApprovalMode::FullAuto.behavior(), ApprovalBehavior::Never);
        assert_eq!(
            crate::capability::ExecutionProfile::for_approval_mode(ApprovalMode::FullAuto),
            crate::capability::ExecutionProfile::Workspace
        );
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Network,
    Agent,
    Shell,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Agent => "agent",
            Self::Shell => "shell",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Prompt,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action: ActionKind,
    pub description: String,
    pub tool: Option<String>,
    pub target: Option<String>,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalResolution {
    pub id: String,
    pub decision: ApprovalDecision,
    pub reason: String,
}
