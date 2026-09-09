use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::cost_types::UsageTotals;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_phase: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceMutationRisk {
    ReadOnlyLikely,
    SourceMutationPossible,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDraft {
    pub draft_id: String,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    pub script: String,
    pub script_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_agent_count: Option<u32>,
    pub max_configured_concurrent_agents: u32,
    pub source_mutation_risk: WorkflowSourceMutationRisk,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDraftActionOutput {
    pub status: String,
    pub action: String,
    pub draft_id: String,
    pub workflow_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOutput {
    pub status: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<WorkflowTokenBudget>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTokenBudget {
    pub total: u64,
    pub spent: u64,
    pub remaining: u64,
}

impl WorkflowTokenBudget {
    pub fn from_total_and_spent(total: u64, spent: u64) -> Self {
        Self {
            total,
            spent,
            remaining: total.saturating_sub(spent),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

pub type WorkflowArgsSchema = BTreeMap<String, WorkflowArgSpec>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArgSpec {
    #[serde(rename = "type")]
    pub arg_type: WorkflowArgType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowArgType {
    String,
    Number,
    Boolean,
    Json,
}

impl WorkflowArgType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Cancelled,
    AsyncLaunched,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentStatus {
    Pending,
    Running,
    Cached,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentFailureKind {
    AgentFailed,
    ToolFailure,
    McpFailure,
    TokenBudget,
    SchemaValidation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMutationPolicy {
    ReadOnly,
    AllowSourceMutation,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceContract {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tool_calls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_tool_failures: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_mcp_failures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_policy: Option<WorkflowMutationPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_observed_concurrency: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceToolEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub is_mcp: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEvidenceFailureKind {
    AgentFailed,
    PhaseFailedContinue,
    PhaseFailedBlocked,
    WorkflowFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseRecord {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub agent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunState {
    pub run_id: String,
    pub task_id: String,
    pub session_id: String,
    pub cwd: String,
    pub workflow_name: String,
    pub meta: WorkflowMeta,
    pub script_digest: String,
    pub args_digest: String,
    pub status: WorkflowRunStatus,
    #[serde(default)]
    pub phases: Vec<WorkflowPhaseRecord>,
    pub total_agent_count: u32,
    pub final_summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceIdentity {
    pub app_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    pub generated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidencePhase {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub agent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskLifecycleEvidence {
    pub task_id: String,
    pub kind: String,
    pub status: String,
    pub turn: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceAgent {
    pub call_id: String,
    pub call_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_hold_ms: Option<u64>,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    #[serde(default)]
    pub previous_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<WorkflowTaskLifecycleEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_events: Vec<WorkflowEvidenceToolEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<WorkflowAgentFailureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub retry_attempted: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceFailure {
    pub kind: WorkflowEvidenceFailureKind,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub retry_attempted: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceBundle {
    pub evidence_version: u32,
    pub identity: WorkflowEvidenceIdentity,
    pub run_id: String,
    pub task_id: String,
    pub session_id: String,
    pub cwd: String,
    pub workflow_name: String,
    pub meta: WorkflowMeta,
    pub script_digest: String,
    pub args_digest: String,
    pub status: WorkflowRunStatus,
    #[serde(default)]
    pub phases: Vec<WorkflowEvidencePhase>,
    pub total_agent_count: u32,
    #[serde(default)]
    pub max_configured_concurrent_agents: u32,
    #[serde(default)]
    pub max_observed_concurrent_agents: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<WorkflowEvidenceContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub agents: Vec<WorkflowEvidenceAgent>,
    #[serde(default)]
    pub failures: Vec<WorkflowEvidenceFailure>,
}
