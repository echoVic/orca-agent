use serde::{Deserialize, Serialize};

use crate::approval_types::ActionKind;
use crate::cost_types::UsageTotals;
use crate::workflow_types::{WorkflowAgentStatus, WorkflowInput, WorkflowRunStatus};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    ApprovalRequired,
    Cancelled,
}

impl TaskStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Running | Self::Paused | Self::Stopping
        )
    }

    pub fn requires_attention(self) -> bool {
        self == Self::ApprovalRequired
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    MainSession,
    Workflow,
    Subagent,
    Shell,
    Monitor,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskProgress {
    pub total_agents: u32,
    pub running_agents: u32,
    pub completed_agents: u32,
    pub failed_agents: u32,
    pub completed_phases: usize,
    pub running_phases: usize,
    pub failed_phases: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentTaskSummary {
    pub call_id: String,
    pub call_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub status: WorkflowAgentStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<TaskContinuationSummary>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseTaskSummary {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub agent_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolCallSummary {
    pub id: String,
    pub name: String,
    pub action: ActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub arguments: String,
}

/// A compact, user-visible activity sample retained for delegated agents.
///
/// This is intentionally bounded at the task-registry boundary. The durable
/// relay remains the complete event source; this list is only the context a
/// task workspace needs to explain what happened most recently.
pub const MAX_SUBAGENT_ACTIVITY_HISTORY: usize = 8;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentActivityEntry {
    pub occurred_at_ms: i64,
    pub activity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

/// Append a compact, bounded activity sample for user-facing task history.
/// Consecutive identical activity/turn pairs are coalesced so relay polling
/// cannot consume the history budget without adding information.
pub fn append_subagent_activity_history(
    history: &mut Vec<SubagentActivityEntry>,
    activity: String,
    turn: Option<u32>,
    occurred_at_ms: i64,
) {
    if history
        .last()
        .is_some_and(|entry| entry.activity == activity && entry.turn == turn)
    {
        return;
    }
    history.push(SubagentActivityEntry {
        occurred_at_ms,
        activity,
        turn,
    });
    if history.len() > MAX_SUBAGENT_ACTIVITY_HISTORY {
        let excess = history.len() - MAX_SUBAGENT_ACTIVITY_HISTORY;
        history.drain(..excess);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskSummary {
    pub id: String,
    /// Stable parent identity used by projected task trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    #[serde(default)]
    pub is_backgrounded: bool,
    pub description: String,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_tool_call: Option<PendingToolCallSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_progress: Option<WorkflowTaskProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_agents: Vec<WorkflowAgentTaskSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_launch_input: Option<WorkflowInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_final_summary: Option<String>,
    #[serde(default)]
    pub workflow_failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_current_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_activity_history: Vec<SubagentActivityEntry>,
    /// Hosted runtime thread that owns the child's interactive transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_child_thread_id: Option<String>,
    /// Stable fan-out identity used to render one launch announcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_batch_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<TaskContinuationSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub output_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_revision: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContinuationSummary {
    pub continuation_id: String,
    pub attempt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    pub revision: u64,
    #[serde(default)]
    pub resumable: bool,
    #[serde(default)]
    pub indeterminate: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskActivitySummary {
    pub active_count: usize,
    pub attention_count: usize,
}

impl TaskActivitySummary {
    pub fn from_tasks(tasks: &[BackgroundTaskSummary]) -> Self {
        tasks.iter().fold(Self::default(), |mut activity, task| {
            if task.status.is_active() {
                activity.active_count += 1;
            }
            if task.status.requires_attention() {
                activity.attention_count += 1;
            }
            activity
        })
    }

    pub fn has_active_tasks(self) -> bool {
        self.active_count > 0
    }

    pub fn requires_attention(self) -> bool {
        self.attention_count > 0
    }
}
