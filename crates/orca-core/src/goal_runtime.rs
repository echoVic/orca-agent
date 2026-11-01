use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

macro_rules! typed_goal_id {
    ($name:ident, $prefix:literal, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, Uuid::now_v7()))
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                let suffix = value
                    .strip_prefix($prefix)
                    .ok_or_else(|| format!("{} must start with {}", $label, $prefix))?;
                let uuid = Uuid::parse_str(suffix)
                    .map_err(|error| format!("invalid {}: {error}", $label))?;
                if uuid.get_version_num() != 7 {
                    return Err(format!("{} must contain a UUIDv7 value", $label));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

typed_goal_id!(GoalId, "goal_", "goal id");
typed_goal_id!(GoalRunId, "goal_run_", "goal run id");
typed_goal_id!(GoalOuterTurnId, "goal_turn_", "goal outer turn id");
typed_goal_id!(IntentId, "goal_intent_", "goal intent id");

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTurnOrigin {
    User,
    Resume,
    Continuation,
    WorkflowNotification,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReason {
    User,
    NoProgress,
    Backoff,
    Infrastructure,
    WaitingForWorkflow,
    Recovery,
    UsageLimit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Test,
    File,
    Command,
    Observation,
    External,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceItem {
    pub kind: EvidenceKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl EvidenceItem {
    pub fn observation(summary: impl Into<String>) -> Self {
        Self {
            kind: EvidenceKind::Observation,
            summary: summary.into(),
            target: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    UserDecision,
    MissingAuthority,
    ExternalState,
    EnvironmentContradiction,
    UnverifiableRequirement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlockerSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    pub kind: BlockerKind,
    pub summary: String,
    pub fingerprint: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalGap {
    pub summary: String,
    pub fingerprint: String,
    pub model_fixable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Paused {
        reason: GoalPauseReason,
        message: String,
    },
    Blocked {
        blocker: BlockerSummary,
    },
    BudgetLimited,
    Complete {
        evidence: Vec<EvidenceItem>,
    },
}

impl GoalState {
    pub fn should_continue(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalRequestedState {
    Complete,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalUpdateIntent {
    pub intent_id: IntentId,
    pub requested_state: GoalRequestedState,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<BlockerSummary>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalRejectCode {
    Inactive,
    NoActiveOuterTurn,
    TerminalIntentPending,
    MissingEvidence,
    MissingBlocker,
    StaleIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "ack", rename_all = "snake_case")]
pub enum GoalUpdateAck {
    DeferredToTurnEnd {
        intent_id: IntentId,
        pending_depth: u32,
    },
    Rejected {
        code: GoalRejectCode,
        message: String,
    },
    AlreadyPending {
        intent_id: IntentId,
    },
    BlockedAgainstInactive {
        state: GoalState,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalUsage {
    pub charged_input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    pub verifier_tokens: i64,
    pub cost_micros: i64,
    pub elapsed_seconds: i64,
}

impl GoalUsage {
    pub fn charged_tokens(&self) -> i64 {
        self.charged_input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.verifier_tokens)
    }

    pub fn saturating_add_assign(&mut self, other: &Self) {
        self.charged_input_tokens = self
            .charged_input_tokens
            .saturating_add(other.charged_input_tokens.max(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(other.output_tokens.max(0));
        self.cache_tokens = self.cache_tokens.saturating_add(other.cache_tokens.max(0));
        self.verifier_tokens = self
            .verifier_tokens
            .saturating_add(other.verifier_tokens.max(0));
        self.cost_micros = self.cost_micros.saturating_add(other.cost_micros.max(0));
        self.elapsed_seconds = self
            .elapsed_seconds
            .saturating_add(other.elapsed_seconds.max(0));
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTurnStatus {
    Success,
    Failed,
    Cancelled,
    ApprovalRequired,
    BudgetExhausted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalOuterTurnSnapshot {
    pub outer_turn_id: GoalOuterTurnId,
    pub goal_run_id: GoalRunId,
    pub origin: GoalTurnOrigin,
    pub model_response_count: u32,
    pub tool_attempt_count: u32,
    pub tool_names: Vec<String>,
    pub status: Option<GoalTurnStatus>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GoalVerificationResult {
    Achieved { evidence: Vec<EvidenceItem> },
    NotAchieved { gaps: Vec<GoalGap> },
    Blocked { blocker: BlockerSummary },
    Indeterminate { message: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalContinuationReason {
    Initial,
    Progress,
    GapFeedback,
    Resume,
    WorkflowNotification,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GoalNextAction {
    Continue {
        reason: GoalContinuationReason,
    },
    Verify {
        intent: GoalUpdateIntent,
    },
    Pause {
        reason: GoalPauseReason,
        message: String,
    },
    Blocked {
        blocker: BlockerSummary,
    },
    BudgetLimited,
    Complete {
        evidence: Vec<EvidenceItem>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalRunSnapshot {
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: Option<GoalOuterTurnId>,
    pub origin: GoalTurnOrigin,
    pub continuation_count: u32,
    pub in_flight: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalTransitionSummary {
    pub previous_state: GoalState,
    pub next_state: GoalState,
    pub reason_code: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalRecord {
    pub goal_id: GoalId,
    pub session_id: String,
    pub objective: String,
    pub objective_revision: u32,
    pub state: GoalState,
    pub token_budget: Option<i64>,
    pub usage: GoalUsage,
    pub current_run: Option<GoalRunSnapshot>,
    pub last_transition: Option<GoalTransitionSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_summary_without_question_serializes_unchanged() {
        let old = serde_json::json!({
            "kind": "user_decision", "summary": "need a port",
            "fingerprint": "port", "evidence": []
        });
        let blocker: BlockerSummary = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(blocker.question, None);
        assert_eq!(serde_json::to_value(blocker).unwrap(), old);
        let explicit = BlockerSummary {
            question: None,
            kind: BlockerKind::UserDecision,
            summary: "need a port".into(),
            fingerprint: "port".into(),
            evidence: vec![],
        };
        assert_eq!(serde_json::to_value(explicit).unwrap(), old);
    }

    #[test]
    fn typed_goal_ids_are_unique_and_round_trip() {
        let goal_id = GoalId::new();
        let run_id = GoalRunId::new();
        let turn_id = GoalOuterTurnId::new();
        let intent_id = IntentId::new();

        assert_eq!(GoalId::parse(goal_id.to_string()).unwrap(), goal_id);
        assert_eq!(GoalRunId::parse(run_id.to_string()).unwrap(), run_id);
        assert_eq!(
            GoalOuterTurnId::parse(turn_id.to_string()).unwrap(),
            turn_id
        );
        assert_eq!(IntentId::parse(intent_id.to_string()).unwrap(), intent_id);
        assert_ne!(GoalId::new(), goal_id);
    }

    #[test]
    fn terminal_intent_round_trips_with_structured_evidence_and_blocker() {
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Blocked,
            reason: "waiting for a user-owned credential".to_string(),
            evidence: vec![EvidenceItem {
                kind: EvidenceKind::Observation,
                summary: "credential lookup returned no value".to_string(),
                target: Some("DEEPSEEK_API_KEY".to_string()),
            }],
            blocker: Some(BlockerSummary {
                question: None,
                kind: BlockerKind::MissingAuthority,
                summary: "the user must provide a credential".to_string(),
                fingerprint: "missing-authority:deepseek-api-key".to_string(),
                evidence: Vec::new(),
            }),
        };

        let encoded = serde_json::to_string(&intent).unwrap();
        let decoded: GoalUpdateIntent = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, intent);
    }
}
