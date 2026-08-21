use std::fmt;

use orca_core::cancel::CancelToken;
use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::goal_runtime::{
    EvidenceItem, GoalGap, GoalRequestedState, GoalState, GoalUpdateIntent, GoalUsage,
    GoalVerificationResult,
};
use orca_core::provider_types::ProviderStep;
use orca_provider::ProviderConfig;
use serde::Deserialize;
use serde_json::json;

const MAX_EVIDENCE_ITEMS: usize = 32;
const MAX_EVIDENCE_SUMMARY_CHARS: usize = 2_000;
const MAX_VERIFIER_INPUT_CHARS: usize = 24_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalVerifierError {
    MissingEvidence,
    MissingBlocker,
    OversizedEvidence,
    ActiveWorkflow,
    MissingTerminalToolResult,
    InFlight,
    BudgetExhausted,
    InvalidBlocker(String),
    InvalidResponse(String),
    Indeterminate(String),
}

impl fmt::Display for GoalVerifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEvidence => formatter.write_str("terminal verification requires evidence"),
            Self::MissingBlocker => formatter.write_str("blocked verification requires a blocker"),
            Self::OversizedEvidence => {
                formatter.write_str("terminal verification evidence is too large")
            }
            Self::ActiveWorkflow => formatter
                .write_str("terminal verification is unavailable while a workflow is active"),
            Self::MissingTerminalToolResult => formatter
                .write_str("terminal verification requires a committed terminal tool result"),
            Self::InFlight => {
                formatter.write_str("terminal verification requires a closed outer turn")
            }
            Self::BudgetExhausted => formatter.write_str("goal token budget is exhausted"),
            Self::InvalidBlocker(message) => write!(formatter, "invalid blocker: {message}"),
            Self::InvalidResponse(message) => {
                write!(formatter, "invalid verifier response: {message}")
            }
            Self::Indeterminate(message) => {
                write!(formatter, "terminal verification indeterminate: {message}")
            }
        }
    }
}

impl std::error::Error for GoalVerifierError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalVerificationRequest {
    pub objective: String,
    pub intent: GoalUpdateIntent,
    pub goal_state: GoalState,
    pub active_workflow: bool,
    pub terminal_tool_result: bool,
    pub outer_turn_in_flight: bool,
    pub budget_remaining: Option<i64>,
    pub viable_model_fixable_gaps: Vec<GoalGap>,
    pub last_model_response: Option<String>,
}

impl GoalVerificationRequest {
    pub fn new(objective: impl Into<String>, intent: GoalUpdateIntent) -> Self {
        Self {
            objective: objective.into(),
            intent,
            goal_state: GoalState::Active,
            active_workflow: false,
            terminal_tool_result: true,
            outer_turn_in_flight: false,
            budget_remaining: None,
            viable_model_fixable_gaps: Vec::new(),
            last_model_response: None,
        }
    }

    pub fn estimated_input_chars(&self) -> usize {
        self.objective.chars().count()
            + self
                .last_model_response
                .as_deref()
                .map(str::chars)
                .map(Iterator::count)
                .unwrap_or_default()
            + self
                .intent
                .evidence
                .iter()
                .map(|item| item.summary.chars().count())
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalVerifierOutput {
    pub result: GoalVerificationResult,
    pub usage: GoalUsage,
}

pub trait GoalVerifier: Send + Sync {
    fn verify(
        &self,
        request: &GoalVerificationRequest,
    ) -> Result<GoalVerifierOutput, GoalVerifierError>;
}

pub fn deterministic_preflight(request: &GoalVerificationRequest) -> Result<(), GoalVerifierError> {
    if !matches!(request.goal_state, GoalState::Active) {
        return Err(GoalVerifierError::InvalidBlocker(
            "goal is not active".to_string(),
        ));
    }
    if request.active_workflow {
        return Err(GoalVerifierError::ActiveWorkflow);
    }
    if request.outer_turn_in_flight {
        return Err(GoalVerifierError::InFlight);
    }
    if !request.terminal_tool_result {
        return Err(GoalVerifierError::MissingTerminalToolResult);
    }
    if request
        .budget_remaining
        .is_some_and(|remaining| remaining <= 0)
    {
        return Err(GoalVerifierError::BudgetExhausted);
    }
    if request.intent.evidence.is_empty() {
        return Err(GoalVerifierError::MissingEvidence);
    }
    if request.intent.evidence.len() > MAX_EVIDENCE_ITEMS
        || request
            .intent
            .evidence
            .iter()
            .any(|evidence| evidence.summary.chars().count() > MAX_EVIDENCE_SUMMARY_CHARS)
        || request.estimated_input_chars() > MAX_VERIFIER_INPUT_CHARS
    {
        return Err(GoalVerifierError::OversizedEvidence);
    }
    if request.intent.requested_state == GoalRequestedState::Blocked
        && request.intent.blocker.is_none()
    {
        return Err(GoalVerifierError::MissingBlocker);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicGoalVerifier;

impl GoalVerifier for DeterministicGoalVerifier {
    fn verify(
        &self,
        request: &GoalVerificationRequest,
    ) -> Result<GoalVerifierOutput, GoalVerifierError> {
        deterministic_preflight(request)?;
        let result = match request.intent.requested_state {
            GoalRequestedState::Complete => GoalVerificationResult::Achieved {
                evidence: request.intent.evidence.clone(),
            },
            GoalRequestedState::Blocked => {
                if let Some(gap) = request.viable_model_fixable_gaps.first().cloned() {
                    GoalVerificationResult::NotAchieved { gaps: vec![gap] }
                } else {
                    let blocker = request
                        .intent
                        .blocker
                        .clone()
                        .ok_or(GoalVerifierError::MissingBlocker)?;
                    GoalVerificationResult::Blocked { blocker }
                }
            }
        };
        Ok(GoalVerifierOutput {
            result,
            usage: GoalUsage::default(),
        })
    }
}

#[derive(Clone)]
pub struct DeepSeekGoalVerifier {
    provider_config: ProviderConfig,
    cancel: CancelToken,
}

impl DeepSeekGoalVerifier {
    pub fn new(provider_config: ProviderConfig, cancel: CancelToken) -> Self {
        Self {
            provider_config,
            cancel,
        }
    }
}

impl GoalVerifier for DeepSeekGoalVerifier {
    fn verify(
        &self,
        request: &GoalVerificationRequest,
    ) -> Result<GoalVerifierOutput, GoalVerifierError> {
        deterministic_preflight(request)?;
        if self.cancel.is_cancelled() {
            return Err(GoalVerifierError::Indeterminate(
                "verifier cancelled before provider request".to_string(),
            ));
        }

        let mut conversation = Conversation::new();
        conversation.add_system(
            "You are a Goal terminal verifier. Return only one JSON object matching this closed schema: "
                .to_string()
                + "{\"outcome\":\"achieved\",\"evidence\":[{\"kind\":\"test|file|command|observation|external\",\"summary\":\"...\",\"target\":\"optional\"}]} or {\"outcome\":\"not_achieved\",\"gaps\":[{\"summary\":\"...\",\"fingerprint\":\"...\",\"model_fixable\":true}]} or {\"outcome\":\"blocked\",\"blocker\":{\"kind\":\"user_decision|missing_authority|external_state|environment_contradiction|unverifiable_requirement\",\"summary\":\"...\",\"fingerprint\":\"...\",\"evidence\":[]}} or {\"outcome\":\"indeterminate\",\"message\":\"...\"}."
        );
        conversation.add_user(
            serde_json::to_string(&json!({
                "objective": request.objective,
                "requested_state": request.intent.requested_state,
                "reason": request.intent.reason,
                "evidence": request.intent.evidence,
                "blocker": request.intent.blocker,
                "viable_model_fixable_gaps": request.viable_model_fixable_gaps,
                "last_model_response": request.last_model_response,
            }))
            .map_err(|error| GoalVerifierError::InvalidResponse(error.to_string()))?,
        );

        let mut provider_config = self.provider_config.clone();
        provider_config.tools_override = Some(Vec::new());
        let response = orca_provider::call_streaming(
            ProviderKind::DeepSeek,
            &conversation,
            &provider_config,
            &self.cancel,
            &mut |_| {},
        );
        let usage = response
            .usage
            .map_or_else(GoalUsage::default, |usage| GoalUsage {
                cache_tokens: usage.cache_tokens as i64,
                verifier_tokens: usage.input_tokens.saturating_add(usage.output_tokens) as i64,
                ..GoalUsage::default()
            });
        if self.cancel.is_cancelled() {
            return Err(GoalVerifierError::Indeterminate(
                "verifier cancelled during provider request".to_string(),
            ));
        }
        if let Some(error) = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(error) => Some(error.clone()),
            _ => None,
        }) {
            return Err(GoalVerifierError::Indeterminate(error.message));
        }
        let content = response
            .assistant_content
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| {
                GoalVerifierError::Indeterminate("verifier returned no JSON".to_string())
            })?;
        let result = parse_deepseek_verifier_response(&content)?;
        Ok(GoalVerifierOutput { result, usage })
    }
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum DeepSeekVerifierResponse {
    Achieved {
        evidence: Vec<EvidenceItem>,
    },
    NotAchieved {
        gaps: Vec<GoalGap>,
    },
    Blocked {
        blocker: orca_core::goal_runtime::BlockerSummary,
    },
    Indeterminate {
        message: String,
    },
}

fn parse_deepseek_verifier_response(
    content: &str,
) -> Result<GoalVerificationResult, GoalVerifierError> {
    let response: DeepSeekVerifierResponse = serde_json::from_str(content)
        .map_err(|error| GoalVerifierError::InvalidResponse(error.to_string()))?;
    Ok(match response {
        DeepSeekVerifierResponse::Achieved { evidence } => {
            GoalVerificationResult::Achieved { evidence }
        }
        DeepSeekVerifierResponse::NotAchieved { gaps } => {
            GoalVerificationResult::NotAchieved { gaps }
        }
        DeepSeekVerifierResponse::Blocked { blocker } => {
            GoalVerificationResult::Blocked { blocker }
        }
        DeepSeekVerifierResponse::Indeterminate { message } => {
            GoalVerificationResult::Indeterminate { message }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_runtime::{
        BlockerKind, BlockerSummary, EvidenceItem, GoalRequestedState, IntentId,
    };

    fn complete_request() -> GoalVerificationRequest {
        GoalVerificationRequest::new(
            "ship it",
            GoalUpdateIntent {
                intent_id: IntentId::new(),
                requested_state: GoalRequestedState::Complete,
                reason: "done".to_string(),
                evidence: vec![EvidenceItem::observation("tests passed")],
                blocker: None,
            },
        )
    }

    fn blocked_request() -> GoalVerificationRequest {
        GoalVerificationRequest::new(
            "ship it",
            GoalUpdateIntent {
                intent_id: IntentId::new(),
                requested_state: GoalRequestedState::Blocked,
                reason: "waiting for authority".to_string(),
                evidence: vec![EvidenceItem::observation("permission denied")],
                blocker: Some(BlockerSummary {
                    kind: BlockerKind::MissingAuthority,
                    summary: "user must grant permission".to_string(),
                    fingerprint: "authority:write".to_string(),
                    evidence: Vec::new(),
                }),
            },
        )
    }

    #[test]
    fn deterministic_verifier_requires_evidence_before_achieved() {
        let verifier = DeterministicGoalVerifier;
        let mut request = complete_request();
        request.intent.evidence.clear();
        assert_eq!(
            verifier.verify(&request).unwrap_err(),
            GoalVerifierError::MissingEvidence
        );
    }

    #[test]
    fn deterministic_verifier_preserves_typed_terminal_result() {
        let output = DeterministicGoalVerifier
            .verify(&complete_request())
            .unwrap();
        assert!(matches!(
            output.result,
            GoalVerificationResult::Achieved { .. }
        ));
    }

    #[test]
    fn active_workflow_is_rejected_before_verification() {
        let verifier = DeterministicGoalVerifier;
        let mut request = complete_request();
        request.active_workflow = true;
        assert_eq!(
            verifier.verify(&request).unwrap_err(),
            GoalVerifierError::ActiveWorkflow
        );
    }

    #[test]
    fn false_blocked_claim_becomes_model_fixable_gap() {
        let verifier = DeterministicGoalVerifier;
        let mut request = blocked_request();
        request.viable_model_fixable_gaps = vec![GoalGap {
            summary: "try the next roadmap slice".to_string(),
            fingerprint: "roadmap:next-slice".to_string(),
            model_fixable: true,
        }];
        assert!(matches!(
            verifier.verify(&request).unwrap().result,
            GoalVerificationResult::NotAchieved { .. }
        ));
    }

    #[test]
    fn verifier_budget_boundary_rejects_before_provider_request() {
        let verifier = DeterministicGoalVerifier;
        let mut request = complete_request();
        request.budget_remaining = Some(0);
        assert_eq!(
            verifier.verify(&request).unwrap_err(),
            GoalVerifierError::BudgetExhausted
        );
    }

    #[test]
    fn deepseek_response_parser_accepts_only_closed_typed_records() {
        let parsed = parse_deepseek_verifier_response(
            r#"{"outcome":"not_achieved","gaps":[{"summary":"next","fingerprint":"gap:next","model_fixable":true}]}"#,
        )
        .unwrap();
        assert!(matches!(parsed, GoalVerificationResult::NotAchieved { .. }));
        assert!(parse_deepseek_verifier_response(
            r#"{"outcome":"blocked","blocker":{"kind":"external_state","summary":"x","fingerprint":"x","evidence":[]},"extra":true}"#,
        )
        .is_err());
    }
}
