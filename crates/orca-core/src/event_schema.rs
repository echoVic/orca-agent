use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval_types::{ApprovalRequest, ApprovalResolution};
use crate::cost_types::UsageTotals;
use crate::goal_runtime::{
    EvidenceItem, GoalId, GoalNextAction, GoalOuterTurnId, GoalPauseReason, GoalRecord, GoalRunId,
    GoalState, GoalTurnOrigin, GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage,
    GoalVerificationResult,
};
use crate::model::ModelRouteDecision;
use crate::plan_types::UpdatePlanArgs;
use crate::provider_types::{ProviderReplayState, ToolCallProgress};
use crate::task_types::BackgroundTaskSummary;
use crate::thread_identity::TurnId;
use crate::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
use crate::tool_types::{FileChangePreview, ToolRequest, ToolResult, ToolTerminalSource};
use crate::verification::VerificationResult;

pub const EVENT_SCHEMA_VERSION: &str = "1";
pub const EVENT_SEQUENCE_RESERVATION_SIZE: u64 = 256;

pub trait EventPublicationStore: Send + Sync {
    fn reserve_through(&self, next_seq_exclusive: u64) -> io::Result<()>;
    fn append_semantic_event(&self, event: &EventEnvelope) -> io::Result<()>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub version: String,
    pub run_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub payload: Value,
}

#[derive(Debug)]
pub struct EventDraft {
    pub run_id: String,
    pub event_type: EventType,
    pub payload: Value,
    publication: Arc<EventPublication>,
}

struct EventPublication {
    state: Mutex<EventPublicationState>,
    store: Option<Arc<dyn EventPublicationStore>>,
}

#[derive(Debug, Default)]
struct EventPublicationState {
    next_seq: u64,
    reserved_until: u64,
}

impl Default for EventPublication {
    fn default() -> Self {
        Self {
            state: Mutex::new(EventPublicationState::default()),
            store: None,
        }
    }
}

impl fmt::Debug for EventPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        formatter
            .debug_struct("EventPublication")
            .field("state", &*state)
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

struct EventPublicationGuard<'a> {
    state: MutexGuard<'a, EventPublicationState>,
}

impl Drop for EventPublicationGuard<'_> {
    fn drop(&mut self) {
        self.state.next_seq += 1;
    }
}

impl EventDraft {
    pub(crate) fn publish<R>(
        self,
        publish: impl FnOnce(&EventEnvelope) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut state = self
            .publication
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.next_seq == u64::MAX {
            return Err(io::Error::other("event sequence exhausted"));
        }
        if let Some(store) = self.publication.store.as_deref()
            && state.next_seq >= state.reserved_until
        {
            let reserved_until = state
                .next_seq
                .checked_add(EVENT_SEQUENCE_RESERVATION_SIZE)
                .ok_or_else(|| io::Error::other("event sequence reservation exhausted"))?;
            store.reserve_through(reserved_until)?;
            state.reserved_until = reserved_until;
        }
        let event = EventEnvelope {
            version: EVENT_SCHEMA_VERSION.to_string(),
            run_id: self.run_id,
            seq: state.next_seq,
            timestamp_ms: timestamp_ms(),
            event_type: self.event_type,
            payload: self.payload,
        };
        let publication = EventPublicationGuard { state };
        let result = (|| {
            if event.event_type.is_semantic()
                && let Some(store) = self.publication.store.as_deref()
            {
                store.append_semantic_event(&event)?;
            }
            publish(&event)
        })();
        drop(publication);
        result
    }

    pub(crate) fn requires_publication_without_observer(&self) -> bool {
        self.event_type.is_semantic() && self.publication.store.is_some()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EventType {
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "assistant.reasoning.delta")]
    AssistantReasoningDelta,
    #[serde(rename = "assistant.message.delta")]
    AssistantMessageDelta,
    #[serde(rename = "model.response.completed")]
    ModelResponseCompleted,
    #[serde(rename = "provider.replay.updated")]
    ProviderReplayUpdated,
    #[serde(rename = "usage.updated")]
    UsageUpdated,
    #[serde(rename = "context.updated")]
    ContextUpdated,
    #[serde(rename = "context.compaction.started")]
    ContextCompactionStarted,
    #[serde(rename = "context.compacted")]
    ContextCompacted,
    #[serde(rename = "model.routed")]
    ModelRouted,
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "tool.call.progress")]
    ToolCallProgress,
    #[serde(rename = "tool.output.delta")]
    ToolOutputDelta,
    #[serde(rename = "tool.call.requested")]
    ToolCallRequested,
    #[serde(rename = "tool.call.completed")]
    ToolCallCompleted,
    #[serde(rename = "plan.updated")]
    PlanUpdated,
    #[serde(rename = "goal.created")]
    GoalCreated,
    #[serde(rename = "goal.run.started")]
    GoalRunStarted,
    #[serde(rename = "goal.turn.started")]
    GoalTurnStarted,
    #[serde(rename = "goal.intent.requested")]
    GoalIntentRequested,
    #[serde(rename = "goal.intent.acknowledged")]
    GoalIntentAcknowledged,
    #[serde(rename = "goal.turn.finished")]
    GoalTurnFinished,
    #[serde(rename = "goal.verification.completed")]
    GoalVerificationCompleted,
    #[serde(rename = "goal.transitioned")]
    GoalTransitioned,
    #[serde(rename = "goal.continuation.admitted")]
    GoalContinuationAdmitted,
    #[serde(rename = "goal.continuation.rejected")]
    GoalContinuationRejected,
    #[serde(rename = "goal.paused")]
    GoalPaused,
    #[serde(rename = "goal.recovered")]
    GoalRecovered,
    #[serde(rename = "goal.completed")]
    GoalCompleted,
    #[serde(rename = "subagent.started")]
    SubagentStarted,
    #[serde(rename = "subagent.progress")]
    SubagentProgress,
    #[serde(rename = "subagent.completed")]
    SubagentCompleted,
    #[serde(rename = "agent.continuation.checkpointed")]
    AgentContinuationCheckpointed,
    #[serde(rename = "agent.continuation.suspended")]
    AgentContinuationSuspended,
    #[serde(rename = "agent.continuation.resumed")]
    AgentContinuationResumed,
    #[serde(rename = "agent.continuation.orphan_reconciled")]
    AgentContinuationOrphanReconciled,
    #[serde(rename = "agent.continuation.indeterminate")]
    AgentContinuationIndeterminate,
    #[serde(rename = "workflow.started")]
    WorkflowStarted,
    #[serde(rename = "workflow.resumed")]
    WorkflowResumed,
    #[serde(rename = "workflow.phase.started")]
    WorkflowPhaseStarted,
    #[serde(rename = "workflow.phase.completed")]
    WorkflowPhaseCompleted,
    #[serde(rename = "workflow.agent.started")]
    WorkflowAgentStarted,
    #[serde(rename = "workflow.agent.cached")]
    WorkflowAgentCached,
    #[serde(rename = "workflow.agent.completed")]
    WorkflowAgentCompleted,
    #[serde(rename = "workflow.agent.failed")]
    WorkflowAgentFailed,
    #[serde(rename = "workflow.paused")]
    WorkflowPaused,
    #[serde(rename = "workflow.stopped")]
    WorkflowStopped,
    #[serde(rename = "workflow.completed")]
    WorkflowCompleted,
    #[serde(rename = "workflow.failed")]
    WorkflowFailed,
    #[serde(rename = "workflow.result.available")]
    WorkflowResultAvailable,
    #[serde(rename = "workflow.tasks.updated")]
    WorkflowTasksUpdated,
    #[serde(rename = "task.status.updated")]
    TaskStatusUpdated,
    #[serde(rename = "verification.started")]
    VerificationStarted,
    #[serde(rename = "verification.completed")]
    VerificationCompleted,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "session.completed")]
    SessionCompleted,
}

impl EventType {
    pub const fn is_semantic(self) -> bool {
        match self {
            Self::SessionStarted
            | Self::TurnStarted
            | Self::ContextCompactionStarted
            | Self::ContextCompacted
            | Self::ModelRouted
            | Self::ApprovalRequested
            | Self::ApprovalResolved
            | Self::ToolCallRequested
            | Self::ToolCallCompleted
            | Self::ModelResponseCompleted
            | Self::PlanUpdated
            | Self::GoalCreated
            | Self::GoalRunStarted
            | Self::GoalTurnStarted
            | Self::GoalIntentRequested
            | Self::GoalIntentAcknowledged
            | Self::GoalTurnFinished
            | Self::GoalVerificationCompleted
            | Self::GoalTransitioned
            | Self::GoalContinuationAdmitted
            | Self::GoalContinuationRejected
            | Self::GoalPaused
            | Self::GoalRecovered
            | Self::GoalCompleted
            | Self::SubagentStarted
            | Self::SubagentCompleted
            | Self::AgentContinuationCheckpointed
            | Self::AgentContinuationSuspended
            | Self::AgentContinuationResumed
            | Self::AgentContinuationOrphanReconciled
            | Self::AgentContinuationIndeterminate
            | Self::WorkflowStarted
            | Self::WorkflowResumed
            | Self::WorkflowPhaseStarted
            | Self::WorkflowPhaseCompleted
            | Self::WorkflowAgentStarted
            | Self::WorkflowAgentCached
            | Self::WorkflowAgentCompleted
            | Self::WorkflowAgentFailed
            | Self::WorkflowPaused
            | Self::WorkflowStopped
            | Self::WorkflowCompleted
            | Self::WorkflowFailed
            | Self::WorkflowResultAvailable
            | Self::VerificationStarted
            | Self::VerificationCompleted
            | Self::Error
            | Self::SessionCompleted => true,
            Self::AssistantReasoningDelta
            | Self::AssistantMessageDelta
            | Self::ProviderReplayUpdated
            | Self::UsageUpdated
            | Self::ContextUpdated
            | Self::ToolCallProgress
            | Self::ToolOutputDelta
            | Self::SubagentProgress
            | Self::WorkflowTasksUpdated
            | Self::TaskStatusUpdated => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Failed,
    Cancelled,
    ApprovalRequired,
    VerificationFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextCompactionStartedPayload {
    pub reason: String,
    pub before_messages: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextCompactedPayload {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub strategy: String,
    pub before_messages: usize,
    pub after_messages: usize,
    #[serde(default)]
    pub collapsed_messages: usize,
    #[serde(default)]
    pub status_text: String,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ApprovalRequired => "approval_required",
            Self::VerificationFailed => "verification_failed",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::VerificationFailed => 2,
            Self::ApprovalRequired => 3,
            Self::Cancelled => 130,
        }
    }
}

pub struct EventFactory {
    run_id: String,
    publication: Arc<EventPublication>,
}

impl EventFactory {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            publication: Arc::new(EventPublication::default()),
        }
    }

    pub fn with_publication_store(
        run_id: String,
        next_seq: u64,
        store: Arc<dyn EventPublicationStore>,
    ) -> Self {
        Self {
            run_id,
            publication: Arc::new(EventPublication {
                state: Mutex::new(EventPublicationState {
                    next_seq,
                    reserved_until: next_seq,
                }),
                store: Some(store),
            }),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Fork an event producer without creating a second sequence authority.
    pub fn fork(&self) -> Self {
        Self {
            run_id: self.run_id.clone(),
            publication: Arc::clone(&self.publication),
        }
    }

    pub fn session_started(
        &mut self,
        cwd: &str,
        approval_mode: &str,
        provider: &str,
        verifier: Option<&str>,
    ) -> EventDraft {
        self.make(
            EventType::SessionStarted,
            json!({
                "cwd": cwd,
                "approval_mode": approval_mode,
                "provider": provider,
                "verifier": verifier
            }),
        )
    }

    pub fn turn_started(
        &mut self,
        turn_id: &TurnId,
        turn: u32,
        prompt: Option<&str>,
    ) -> EventDraft {
        let mut payload = json!({
            "turn_id": turn_id,
            "turn": turn
        });
        if let Some(p) = prompt {
            payload["prompt"] = json!(p);
        }
        self.make(EventType::TurnStarted, payload)
    }

    pub fn assistant_reasoning_delta(
        &mut self,
        identity: &ModelResponseIdentity,
        text: &str,
    ) -> EventDraft {
        self.make(
            EventType::AssistantReasoningDelta,
            json!({
                "turn_id": identity.turn_id,
                "item_id": identity.item_ids.reasoning_item_id,
                "text": text
            }),
        )
    }

    pub fn assistant_message_delta(
        &mut self,
        identity: &ModelResponseIdentity,
        text: &str,
    ) -> EventDraft {
        self.make(
            EventType::AssistantMessageDelta,
            json!({
                "turn_id": identity.turn_id,
                "agent_message_item_id": identity.item_ids.agent_message_item_id(),
                "plan_item_id": identity.item_ids.plan_item_id,
                "text": text
            }),
        )
    }

    pub fn model_response_completed(&mut self, response: &CompletedModelResponse) -> EventDraft {
        self.make_serialized(EventType::ModelResponseCompleted, response)
    }

    pub fn provider_replay_updated(&mut self, replay: &ProviderReplayState) -> EventDraft {
        self.make(
            EventType::ProviderReplayUpdated,
            json!({
                "provider": replay.provider,
                "reasoning_content": replay.reasoning_content,
                "tool_call_ids": replay.tool_call_ids
            }),
        )
    }

    pub fn usage_updated(&mut self, usage: UsageTotals) -> EventDraft {
        self.make(
            EventType::UsageUpdated,
            json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_tokens": usage.cache_tokens,
                "total_tokens": usage.total_tokens(),
                "estimated_cost_usd": usage.estimated_cost_usd
            }),
        )
    }

    pub fn context_updated(&mut self, used_tokens: usize, limit_tokens: usize) -> EventDraft {
        self.make(
            EventType::ContextUpdated,
            json!({
                "used_tokens": used_tokens,
                "limit_tokens": limit_tokens
            }),
        )
    }

    pub fn context_compaction_started(
        &mut self,
        reason: &str,
        before_messages: usize,
    ) -> EventDraft {
        self.make_serialized(
            EventType::ContextCompactionStarted,
            ContextCompactionStartedPayload {
                reason: reason.to_string(),
                before_messages,
            },
        )
    }

    pub fn context_compacted(
        &mut self,
        reason: &str,
        strategy: &str,
        before_messages: usize,
        after_messages: usize,
        collapsed_messages: usize,
        status_text: &str,
    ) -> EventDraft {
        self.make_serialized(
            EventType::ContextCompacted,
            ContextCompactedPayload {
                reason: reason.to_string(),
                strategy: strategy.to_string(),
                before_messages,
                after_messages,
                collapsed_messages,
                status_text: status_text.to_string(),
            },
        )
    }

    pub fn model_routed(&mut self, decision: &ModelRouteDecision) -> EventDraft {
        self.make(
            EventType::ModelRouted,
            json!({
                "requested_model": decision.requested_model,
                "actual_model": decision.actual_model,
                "reason": decision.reason,
                "image_route": decision.image_route
            }),
        )
    }

    pub fn approval_requested(&mut self, request: &ApprovalRequest) -> EventDraft {
        self.make(
            EventType::ApprovalRequested,
            json!({
                "id": request.id,
                "action": request.action,
                "description": request.description,
                "tool": request.tool,
                "target": request.target,
                "preview": request.preview
            }),
        )
    }

    pub fn approval_resolved(&mut self, resolution: &ApprovalResolution) -> EventDraft {
        self.make(
            EventType::ApprovalResolved,
            json!({
                "id": resolution.id,
                "decision": resolution.decision,
                "reason": resolution.reason
            }),
        )
    }

    pub fn tool_call_progress(&mut self, progress: &ToolCallProgress) -> EventDraft {
        self.make(
            EventType::ToolCallProgress,
            json!({
                "id": progress.id,
                "name": progress.function_name,
                "arguments_bytes": progress.arguments_bytes
            }),
        )
    }

    pub fn tool_output_delta(&mut self, id: &str, chunk: &str) -> EventDraft {
        self.make(
            EventType::ToolOutputDelta,
            json!({
                "id": id,
                "chunk": chunk
            }),
        )
    }

    pub fn tool_call_requested(&mut self, request: &ToolRequest) -> EventDraft {
        self.make(
            EventType::ToolCallRequested,
            json!({
                "id": request.id,
                "name": request.name,
                "action": request.action,
                "target": request.target,
                "raw_arguments": request.raw_arguments
            }),
        )
    }

    pub fn tool_call_completed(&mut self, result: &ToolResult) -> EventDraft {
        let mut payload = json!({
            "id": result.id,
            "name": result.name,
            "status": result.status,
            "output": result.output,
            "error": result.error,
            "exit_code": result.exit_code,
            "truncated": result.truncated,
            "kind": result.kind
        });
        if result.terminal().source != ToolTerminalSource::Observed {
            payload["terminal_source"] = json!(result.terminal().source);
        }
        if let Some(preview) = result.file_change_preview.as_deref() {
            payload["diff"] = json!(match preview {
                FileChangePreview::UnifiedDiff { text, .. } => text.clone(),
                FileChangePreview::Omitted {
                    path,
                    max_input_bytes,
                } => format!(
                    "[Diff preview omitted for {path}: input exceeds {max_input_bytes} bytes]"
                ),
            });
        }
        self.make(EventType::ToolCallCompleted, payload)
    }

    pub fn plan_updated(&mut self, update: &UpdatePlanArgs) -> EventDraft {
        self.make(EventType::PlanUpdated, json!(update))
    }

    pub fn goal_created(&mut self, goal: &GoalRecord) -> EventDraft {
        self.make_serialized(EventType::GoalCreated, goal)
    }

    pub fn goal_run_started(&mut self, goal_id: &GoalId, goal_run_id: &GoalRunId) -> EventDraft {
        self.make(
            EventType::GoalRunStarted,
            json!({ "goal_id": goal_id, "goal_run_id": goal_run_id }),
        )
    }

    pub fn goal_turn_started(
        &mut self,
        goal_id: &GoalId,
        goal_run_id: &GoalRunId,
        outer_turn_id: &GoalOuterTurnId,
        origin: GoalTurnOrigin,
    ) -> EventDraft {
        self.make(
            EventType::GoalTurnStarted,
            json!({
                "goal_id": goal_id,
                "goal_run_id": goal_run_id,
                "outer_turn_id": outer_turn_id,
                "origin": origin
            }),
        )
    }

    pub fn goal_intent_acknowledged(
        &mut self,
        outer_turn_id: &GoalOuterTurnId,
        intent: &GoalUpdateIntent,
        ack: &GoalUpdateAck,
    ) -> EventDraft {
        self.make(
            EventType::GoalIntentAcknowledged,
            json!({
                "outer_turn_id": outer_turn_id,
                "intent": intent,
                "ack": ack
            }),
        )
    }

    pub fn goal_intent_requested(
        &mut self,
        outer_turn_id: &GoalOuterTurnId,
        intent: &GoalUpdateIntent,
    ) -> EventDraft {
        self.make(
            EventType::GoalIntentRequested,
            json!({
                "intent_id": intent.intent_id,
                "requested_state": intent.requested_state,
                "outer_turn_id": outer_turn_id,
                "reason": intent.reason,
                "evidence": intent.evidence,
                "blocker": intent.blocker,
            }),
        )
    }

    pub fn goal_turn_finished(
        &mut self,
        outer_turn_id: &GoalOuterTurnId,
        status: GoalTurnStatus,
        usage: &GoalUsage,
        next_action: &GoalNextAction,
    ) -> EventDraft {
        self.make(
            EventType::GoalTurnFinished,
            json!({
                "outer_turn_id": outer_turn_id,
                "status": status,
                "usage": usage,
                "next_action": next_action
            }),
        )
    }

    pub fn goal_verification_completed(
        &mut self,
        outer_turn_id: &GoalOuterTurnId,
        result: &GoalVerificationResult,
    ) -> EventDraft {
        self.make(
            EventType::GoalVerificationCompleted,
            json!({ "outer_turn_id": outer_turn_id, "result": result }),
        )
    }

    pub fn goal_transitioned(
        &mut self,
        goal_id: &GoalId,
        previous: &GoalState,
        next: &GoalState,
        reason_code: &str,
    ) -> EventDraft {
        self.make(
            EventType::GoalTransitioned,
            json!({
                "goal_id": goal_id,
                "previous_state": previous,
                "next_state": next,
                "reason_code": reason_code
            }),
        )
    }

    pub fn goal_continuation_admission(
        &mut self,
        goal_id: &GoalId,
        goal_run_id: Option<&GoalRunId>,
        outer_turn_id: Option<&GoalOuterTurnId>,
        admitted: bool,
        reason: &str,
        current_state: &GoalState,
        continuation_count: u32,
    ) -> EventDraft {
        let event_type = if admitted {
            EventType::GoalContinuationAdmitted
        } else {
            EventType::GoalContinuationRejected
        };
        self.make(
            event_type,
            json!({
                "goal_id": goal_id,
                "goal_run_id": goal_run_id,
                "outer_turn_id": outer_turn_id,
                "admitted": admitted,
                "reason": reason,
                "admission_reason": admitted.then_some(reason),
                "rejection_code": (!admitted).then_some(reason),
                "current_state": current_state,
                "counters": {
                    "continuation_count": continuation_count,
                }
            }),
        )
    }

    pub fn goal_paused(
        &mut self,
        goal_id: &GoalId,
        goal_run_id: Option<&GoalRunId>,
        outer_turn_id: Option<&GoalOuterTurnId>,
        reason: GoalPauseReason,
        message: &str,
    ) -> EventDraft {
        self.make(
            EventType::GoalPaused,
            json!({
                "goal_id": goal_id,
                "goal_run_id": goal_run_id,
                "outer_turn_id": outer_turn_id,
                "reason": reason,
                "message": message,
            }),
        )
    }

    pub fn goal_recovered(
        &mut self,
        goal_id: &GoalId,
        stale_goal_run_id: &GoalRunId,
        outer_turn_id: Option<&GoalOuterTurnId>,
        recovered_state: &GoalState,
    ) -> EventDraft {
        self.make(
            EventType::GoalRecovered,
            json!({
                "goal_id": goal_id,
                "stale_goal_run_id": stale_goal_run_id,
                "outer_turn_id": outer_turn_id,
                "recovered_state": recovered_state,
                "discarded_continuation": true,
            }),
        )
    }

    pub fn goal_completed(
        &mut self,
        goal_id: &GoalId,
        goal_run_id: Option<&GoalRunId>,
        evidence: &[EvidenceItem],
        usage: &GoalUsage,
    ) -> EventDraft {
        self.make(
            EventType::GoalCompleted,
            json!({
                "goal_id": goal_id,
                "goal_run_id": goal_run_id,
                "evidence": evidence,
                "usage": usage,
            }),
        )
    }

    pub fn subagent_started(&mut self, id: &str, description: &str) -> EventDraft {
        self.make(
            EventType::SubagentStarted,
            json!({
                "id": id,
                "description": description
            }),
        )
    }

    pub fn subagent_progress(
        &mut self,
        id: &str,
        description: &str,
        activity: &str,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    ) -> EventDraft {
        self.make(
            EventType::SubagentProgress,
            json!({
                "id": id,
                "description": description,
                "activity": activity,
                "turn": turn,
                "usage": usage.map(|usage| json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_tokens": usage.cache_tokens,
                    "total_tokens": usage.total_tokens(),
                    "estimated_cost_usd": usage.estimated_cost_usd
                }))
            }),
        )
    }

    pub fn subagent_completed(
        &mut self,
        id: &str,
        description: &str,
        status: RunStatus,
        output: Option<&str>,
        error: Option<&str>,
    ) -> EventDraft {
        self.make(
            EventType::SubagentCompleted,
            json!({
                "id": id,
                "description": description,
                "status": status,
                "output": output,
                "error": error
            }),
        )
    }

    pub fn workflow_started(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        phases: &[String],
    ) -> EventDraft {
        self.make(
            EventType::WorkflowStarted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "phases": phases
            }),
        )
    }

    pub fn workflow_resumed(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowResumed,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name
            }),
        )
    }

    pub fn workflow_phase_started(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowPhaseStarted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase
            }),
        )
    }

    pub fn workflow_phase_completed(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        status: RunStatus,
        summary: Option<&str>,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowPhaseCompleted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "status": status,
                "summary": summary
            }),
        )
    }

    pub fn workflow_agent_started(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        agent_id: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowAgentStarted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "agentId": agent_id
            }),
        )
    }

    pub fn workflow_agent_cached(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        agent_id: &str,
        output: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowAgentCached,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "agentId": agent_id,
                "output": output
            }),
        )
    }

    pub fn workflow_agent_completed(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        agent_id: &str,
        output: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowAgentCompleted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "agentId": agent_id,
                "output": output
            }),
        )
    }

    pub fn workflow_agent_failed(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        agent_id: &str,
        error: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowAgentFailed,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "agentId": agent_id,
                "error": error
            }),
        )
    }

    pub fn workflow_paused(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        reason: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowPaused,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "reason": reason
            }),
        )
    }

    pub fn workflow_stopped(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        reason: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowStopped,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "reason": reason
            }),
        )
    }

    pub fn workflow_completed(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowCompleted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name
            }),
        )
    }

    pub fn workflow_result_available(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        tool_use_id: Option<&str>,
        status: &str,
        result: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowResultAvailable,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "toolUseId": tool_use_id,
                "status": status,
                "result": result
            }),
        )
    }

    pub fn workflow_failed(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        tool_use_id: Option<&str>,
        error: &str,
    ) -> EventDraft {
        self.make(
            EventType::WorkflowFailed,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "toolUseId": tool_use_id,
                "status": "failed",
                "error": error
            }),
        )
    }

    pub fn workflow_tasks_updated(&mut self, tasks: &[BackgroundTaskSummary]) -> EventDraft {
        self.make(
            EventType::WorkflowTasksUpdated,
            json!({
                "tasks": tasks
            }),
        )
    }

    pub fn task_status_updated(&mut self, task: &BackgroundTaskSummary) -> EventDraft {
        self.make(
            EventType::TaskStatusUpdated,
            json!({
                "task": task
            }),
        )
    }

    pub fn agent_continuation_updated(
        &mut self,
        event_type: EventType,
        task_id: &str,
        continuation: &crate::task_types::TaskContinuationSummary,
    ) -> EventDraft {
        debug_assert!(matches!(
            event_type,
            EventType::AgentContinuationCheckpointed
                | EventType::AgentContinuationSuspended
                | EventType::AgentContinuationResumed
                | EventType::AgentContinuationOrphanReconciled
                | EventType::AgentContinuationIndeterminate
        ));
        self.make(
            event_type,
            json!({
                "taskId": task_id,
                "continuation": continuation,
            }),
        )
    }

    pub fn verification_started(&mut self, command: &str) -> EventDraft {
        self.make(
            EventType::VerificationStarted,
            json!({
                "command": command
            }),
        )
    }

    pub fn verification_completed(&mut self, result: &VerificationResult) -> EventDraft {
        self.make(
            EventType::VerificationCompleted,
            json!({
                "command": result.command,
                "success": result.success,
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr
            }),
        )
    }

    pub fn error(&mut self, message: &str) -> EventDraft {
        self.make(
            EventType::Error,
            json!({
                "message": message
            }),
        )
    }

    pub fn session_completed(&mut self, status: RunStatus, session_id: Option<&str>) -> EventDraft {
        self.session_completed_with_verification(status, session_id, None)
    }

    pub fn session_completed_with_verification(
        &mut self,
        status: RunStatus,
        session_id: Option<&str>,
        verification: Option<&VerificationResult>,
    ) -> EventDraft {
        let mut payload = json!({
            "status": status
        });
        insert_completion_proof(&mut payload, verification);
        if let Some(session_id) = session_id {
            payload["session_id"] = json!(session_id);
        }
        self.make(EventType::SessionCompleted, payload)
    }

    /// Terminal-complete session event carrying the typed operation terminal.
    /// The status string is a projection of the terminal (never an invented
    /// fact), and the typed terminal object rides along for adapters.
    pub fn session_completed_terminal(
        &mut self,
        terminal: &crate::budget::OperationTerminal,
        session_id: Option<&str>,
    ) -> EventDraft {
        self.session_completed_terminal_with_verification(terminal, session_id, None)
    }

    /// Terminal-complete session event carrying the exact verifier result used
    /// by the runtime completion proof. Keeping this on the terminal event
    /// lets JSONL consumers compare the evidence without reconstructing it
    /// from status strings.
    pub fn session_completed_terminal_with_verification(
        &mut self,
        terminal: &crate::budget::OperationTerminal,
        session_id: Option<&str>,
        verification: Option<&VerificationResult>,
    ) -> EventDraft {
        let status = match terminal {
            crate::budget::OperationTerminal::Completed { .. } => "success",
            crate::budget::OperationTerminal::Stopped { .. } => "budget_exhausted",
            crate::budget::OperationTerminal::Failed { .. } => "failed",
            crate::budget::OperationTerminal::Cancelled { .. } => "cancelled",
        };
        let mut payload = json!({
            "status": status,
            "terminal": terminal,
        });
        insert_completion_proof(&mut payload, verification);
        if let Some(session_id) = session_id {
            payload["session_id"] = json!(session_id);
        }
        self.make(EventType::SessionCompleted, payload)
    }

    fn make(&mut self, event_type: EventType, payload: Value) -> EventDraft {
        EventDraft {
            run_id: self.run_id.clone(),
            event_type,
            payload,
            publication: Arc::clone(&self.publication),
        }
    }

    fn make_serialized(&mut self, event_type: EventType, payload: impl Serialize) -> EventDraft {
        self.make(
            event_type,
            serde_json::to_value(payload).expect("runtime event payload serializes"),
        )
    }
}

fn insert_completion_proof(payload: &mut Value, verification: Option<&VerificationResult>) {
    let verification = verification.map_or_else(
        || json!({ "state": "unverified" }),
        |verification| {
            json!({
                "state": if verification.success { "verified" } else { "failed" },
                "result": {
                "command": verification.command,
                "success": verification.success,
                "exit_code": verification.exit_code,
                "stdout": verification.stdout,
                "stderr": verification.stderr,
                }
            })
        },
    );
    payload["completion_proof"] = json!({ "verification": verification });
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputFormat;
    use crate::event_sink::{EventObserver, EventSink};
    use crate::task_types::{BackgroundTaskSummary, TaskStatus, TaskType, WorkflowTaskProgress};
    use crate::tool_types::{ToolName, ToolRequest};

    fn publish(drafts: impl IntoIterator<Item = EventDraft>) -> Vec<EventEnvelope> {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let mut sink = EventSink::new(std::io::sink(), OutputFormat::Jsonl).with_observer(observer);
        for draft in drafts {
            sink.emit(draft).unwrap();
        }
        drop(sink);
        Arc::try_unwrap(observed).unwrap().into_inner().unwrap()
    }

    #[test]
    fn factory_increments_seq() {
        let mut f = EventFactory::new("run-1".to_string());
        let events = publish([f.error("a"), f.error("b"), f.error("c")]);
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn factory_preserves_run_id() {
        let mut f = EventFactory::new("run-abc".to_string());
        let e = f.error("x");
        assert_eq!(e.run_id, "run-abc");
    }

    #[test]
    fn event_type_serializes_correctly() {
        let s = serde_json::to_string(&EventType::SessionStarted).unwrap();
        assert_eq!(s, "\"session.started\"");

        let s = serde_json::to_string(&EventType::ToolCallCompleted).unwrap();
        assert_eq!(s, "\"tool.call.completed\"");

        let s = serde_json::to_string(&EventType::PlanUpdated).unwrap();
        assert_eq!(s, "\"plan.updated\"");
    }

    #[test]
    fn run_status_exit_codes() {
        assert_eq!(RunStatus::Success.exit_code(), 0);
        assert_eq!(RunStatus::Failed.exit_code(), 1);
        assert_eq!(RunStatus::VerificationFailed.exit_code(), 2);
        assert_eq!(RunStatus::ApprovalRequired.exit_code(), 3);
        assert_eq!(RunStatus::Cancelled.exit_code(), 130);
        // Budget stops carry exit code 4 through the typed terminal.
        assert_eq!(
            crate::budget::OperationTerminal::Stopped {
                reason: crate::budget::StopReason::TurnBudget { max_turns: 3 },
                usage: crate::budget::BudgetUsage::default(),
                checkpoint_id: String::new(),
                resumable: false,
            }
            .exit_code(),
            4
        );
    }

    #[test]
    fn tool_terminal_event_serializes_cancelled_status_and_kind() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: crate::approval_types::ActionKind::Shell,
            target: Some("sleep 30".to_string()),
            raw_arguments: None,
        };
        let cancelled = ToolResult::cancelled(&request, "turn interrupted", Some(130));
        let mut events = EventFactory::new("run-1".to_string());

        let value = events.tool_call_completed(&cancelled).payload;

        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["kind"], "cancelled");
        assert!(value.get("terminal_source").is_none());
        assert_eq!(value["error"], "turn interrupted");
        assert_eq!(value["exit_code"], 130);

        let repaired = ToolResult::indeterminate(&request, "missing terminal result")
            .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
        let value = events.tool_call_completed(&repaired).payload;
        assert_eq!(value["status"], "indeterminate");
        assert_eq!(value["terminal_source"], "compatibility_repair");
    }

    #[test]
    fn observed_tool_terminal_keeps_legacy_event_payload_shape() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::ReadFile,
            action: crate::approval_types::ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };
        let completed = ToolResult::completed(&request, "hello".to_string(), false);
        let mut events = EventFactory::new("run-1".to_string());

        let event = events.tool_call_completed(&completed);

        assert_eq!(
            event.payload,
            json!({
                "id": "call-1",
                "name": "read_file",
                "status": "completed",
                "output": "hello",
                "error": null,
                "exit_code": 0,
                "truncated": false,
                "kind": "success"
            })
        );
    }

    #[test]
    fn session_started_payload_structure() {
        let mut f = EventFactory::new("run-1".to_string());
        let e = f.session_started("/tmp", "read-only", "mock", None);
        assert_eq!(e.event_type, EventType::SessionStarted);
        assert_eq!(e.payload["cwd"], "/tmp");
        assert_eq!(e.payload["approval_mode"], "read-only");
        assert_eq!(e.payload["provider"], "mock");
        assert!(e.payload["verifier"].is_null());
    }

    #[test]
    fn session_completed_payload_carries_durable_session_id_when_present() {
        let mut f = EventFactory::new("run-1".to_string());

        let with_session = f.session_completed(RunStatus::Failed, Some("session-9"));
        assert_eq!(with_session.payload["status"], "failed");
        assert_eq!(with_session.payload["session_id"], "session-9");

        let without_session = f.session_completed(RunStatus::Success, None);
        assert_eq!(without_session.payload["status"], "success");
        assert!(without_session.payload["session_id"].is_null());
    }

    #[test]
    fn turn_started_with_and_without_prompt() {
        let mut f = EventFactory::new("run-1".to_string());
        let turn_id = TurnId::new();

        let e = f.turn_started(&turn_id, 1, Some("hello"));
        assert_eq!(e.payload["turn_id"], turn_id.as_str());
        assert_eq!(e.payload["turn"], 1);
        assert_eq!(e.payload["prompt"], "hello");

        let e = f.turn_started(&turn_id, 2, None);
        assert_eq!(e.payload["turn_id"], turn_id.as_str());
        assert_eq!(e.payload["turn"], 2);
        assert!(e.payload.get("prompt").is_none());
    }

    #[test]
    fn approval_requested_payload_includes_display_metadata() {
        let mut f = EventFactory::new("run-1".to_string());
        let request = ApprovalRequest {
            id: "approval-1".to_string(),
            action: crate::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };

        let event = f.approval_requested(&request);

        assert_eq!(event.event_type, EventType::ApprovalRequested);
        assert_eq!(event.payload["id"], "approval-1");
        assert_eq!(event.payload["action"], "shell");
        assert_eq!(event.payload["description"], "bash requested shell");
        assert_eq!(event.payload["tool"], "bash");
        assert_eq!(event.payload["target"], "echo hi");
        assert_eq!(event.payload["preview"], "$ echo hi");
    }

    #[test]
    fn workflow_result_payload_includes_tui_notification_metadata() {
        let mut f = EventFactory::new("run-1".to_string());

        let event = f.workflow_result_available(
            "task-1",
            "workflow-run-1",
            "mock-workflow",
            Some("tool-use-1"),
            "completed",
            "all phases passed",
        );

        assert_eq!(event.event_type, EventType::WorkflowResultAvailable);
        assert_eq!(event.payload["taskId"], "task-1");
        assert_eq!(event.payload["runId"], "workflow-run-1");
        assert_eq!(event.payload["workflowName"], "mock-workflow");
        assert_eq!(event.payload["toolUseId"], "tool-use-1");
        assert_eq!(event.payload["status"], "completed");
        assert_eq!(event.payload["result"], "all phases passed");
    }

    #[test]
    fn workflow_lifecycle_payloads_cover_declared_event_types() {
        let mut f = EventFactory::new("run-1".to_string());

        let resumed = f.workflow_resumed("task-1", "workflow-run-1", "audit");
        assert_eq!(resumed.event_type, EventType::WorkflowResumed);
        assert_eq!(resumed.payload["taskId"], "task-1");
        assert_eq!(resumed.payload["runId"], "workflow-run-1");
        assert_eq!(resumed.payload["workflowName"], "audit");

        let phase_started = f.workflow_phase_started("task-1", "workflow-run-1", "scan");
        assert_eq!(phase_started.event_type, EventType::WorkflowPhaseStarted);
        assert_eq!(phase_started.payload["phase"], "scan");

        let phase_completed = f.workflow_phase_completed(
            "task-1",
            "workflow-run-1",
            "scan",
            RunStatus::Success,
            Some("scan ok"),
        );
        assert_eq!(
            phase_completed.event_type,
            EventType::WorkflowPhaseCompleted
        );
        assert_eq!(phase_completed.payload["status"], "success");
        assert_eq!(phase_completed.payload["summary"], "scan ok");

        let agent_started = f.workflow_agent_started("task-1", "workflow-run-1", "scan", "agent-1");
        assert_eq!(agent_started.event_type, EventType::WorkflowAgentStarted);
        assert_eq!(agent_started.payload["agentId"], "agent-1");

        let agent_cached =
            f.workflow_agent_cached("task-1", "workflow-run-1", "scan", "agent-1", "cached ok");
        assert_eq!(agent_cached.event_type, EventType::WorkflowAgentCached);
        assert_eq!(agent_cached.payload["output"], "cached ok");

        let agent_failed =
            f.workflow_agent_failed("task-1", "workflow-run-1", "scan", "agent-1", "boom");
        assert_eq!(agent_failed.event_type, EventType::WorkflowAgentFailed);
        assert_eq!(agent_failed.payload["error"], "boom");

        let paused = f.workflow_paused("task-1", "workflow-run-1", "audit", "user pause");
        assert_eq!(paused.event_type, EventType::WorkflowPaused);
        assert_eq!(paused.payload["reason"], "user pause");

        let stopped = f.workflow_stopped("task-1", "workflow-run-1", "audit", "stop requested");
        assert_eq!(stopped.event_type, EventType::WorkflowStopped);
        assert_eq!(stopped.payload["reason"], "stop requested");
    }

    #[test]
    fn workflow_tasks_updated_payload_includes_task_summaries() {
        let mut f = EventFactory::new("run-1".to_string());
        let task = BackgroundTaskSummary {
            id: "task-1".to_string(),
            parent_task_id: None,
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "demo workflow".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("workflow".to_string()),
            pending_tool_call: None,
            name: Some("demo".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: Some(WorkflowTaskProgress {
                total_agents: 3,
                running_agents: 1,
                completed_agents: 2,
                failed_agents: 0,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some("workflow.md".to_string()),
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        let event = f.workflow_tasks_updated(&[task]);

        assert_eq!(event.event_type, EventType::WorkflowTasksUpdated);
        assert_eq!(event.payload["tasks"][0]["id"], "task-1");
        assert_eq!(event.payload["tasks"][0]["type"], "workflow");
        assert_eq!(event.payload["tasks"][0]["status"], "running");
        assert_eq!(event.payload["tasks"][0]["workflowRunId"], "workflow-run-1");
        assert_eq!(
            event.payload["tasks"][0]["workflowProgress"]["completedAgents"],
            2
        );
    }

    #[test]
    fn task_status_updated_payload_includes_single_task_summary() {
        let mut f = EventFactory::new("run-1".to_string());
        let task = BackgroundTaskSummary {
            id: "main-session-1".to_string(),
            parent_task_id: None,
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "background turn".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("shell".to_string()),
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_turn: None,
            last_activity_at_ms: Some(30),
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        let event = f.task_status_updated(&task);

        assert_eq!(event.event_type, EventType::TaskStatusUpdated);
        assert_eq!(event.payload["task"]["id"], "main-session-1");
        assert_eq!(event.payload["task"]["type"], "main_session");
        assert_eq!(event.payload["task"]["status"], "approval_required");
        assert_eq!(event.payload["task"]["isBackgrounded"], true);
        assert_eq!(event.payload["task"]["tool"], "shell");
    }

    #[test]
    fn context_compacted_payload_includes_projection_details() {
        let mut f = EventFactory::new("run-1".to_string());

        let event = f.context_compacted(
            "prompt_too_long_recovery",
            "remote_summary",
            12,
            5,
            7,
            "compacted context after prompt-too-long",
        );

        assert_eq!(event.event_type, EventType::ContextCompacted);
        assert_eq!(event.payload["reason"], "prompt_too_long_recovery");
        assert_eq!(event.payload["strategy"], "remote_summary");
        assert_eq!(event.payload["before_messages"], 12);
        assert_eq!(event.payload["after_messages"], 5);
        assert_eq!(event.payload["collapsed_messages"], 7);
        assert_eq!(
            event.payload["status_text"],
            "compacted context after prompt-too-long"
        );
    }

    #[test]
    fn context_compaction_started_payload_includes_trigger_and_message_count() {
        let mut f = EventFactory::new("run-1".to_string());

        let event = f.context_compaction_started("prompt_too_long_recovery", 12);

        assert_eq!(event.event_type, EventType::ContextCompactionStarted);
        assert_eq!(event.payload["reason"], "prompt_too_long_recovery");
        assert_eq!(event.payload["before_messages"], 12);
    }

    #[test]
    fn context_updated_payload_carries_wire_budget() {
        let mut events = EventFactory::new("context-budget-test".to_string());

        let event = events.context_updated(12_345, 96_000);

        assert_eq!(event.event_type, EventType::ContextUpdated);
        assert_eq!(event.payload["used_tokens"], 12_345);
        assert_eq!(event.payload["limit_tokens"], 96_000);
    }

    #[test]
    fn tool_output_delta_payload_carries_call_identity_and_chunk() {
        let mut events = EventFactory::new("tool-output-test".to_string());

        let event = events.tool_output_delta("call-1", "before\n");

        assert_eq!(event.event_type, EventType::ToolOutputDelta);
        assert_eq!(event.payload["id"], "call-1");
        assert_eq!(event.payload["chunk"], "before\n");
    }

    #[test]
    fn tool_completed_payload_carries_committed_file_preview() {
        let request = ToolRequest {
            id: "edit-1".to_string(),
            name: crate::tool_types::ToolName::Edit,
            action: crate::approval_types::ActionKind::Write,
            target: Some("notes.txt".to_string()),
            raw_arguments: None,
        };
        let result = ToolResult::completed(&request, "edited notes.txt".to_string(), false)
            .with_file_change_preview(crate::tool_types::FileChangePreview::UnifiedDiff {
                text: "--- a/notes.txt\n+++ b/notes.txt\n-old\n+new\n".to_string(),
                truncated: false,
            });
        let mut events = EventFactory::new("tool-preview-test".to_string());

        let event = events.tool_call_completed(&result);

        assert_eq!(
            event.payload["diff"],
            "--- a/notes.txt\n+++ b/notes.txt\n-old\n+new\n"
        );
    }

    #[test]
    fn goal_events_are_typed_semantic_records() {
        let mut events = EventFactory::new("goal-events".to_string());
        let goal_id = GoalId::new();
        let run_id = GoalRunId::new();
        let turn_id = GoalOuterTurnId::new();
        let intent = GoalUpdateIntent {
            intent_id: crate::goal_runtime::IntentId::new(),
            requested_state: crate::goal_runtime::GoalRequestedState::Complete,
            reason: "tests passed".to_string(),
            evidence: vec![crate::goal_runtime::EvidenceItem::observation(
                "focused tests passed",
            )],
            blocker: None,
        };

        let started =
            events.goal_turn_started(&goal_id, &run_id, &turn_id, GoalTurnOrigin::Continuation);
        let requested = events.goal_intent_requested(&turn_id, &intent);
        let admission = events.goal_continuation_admission(
            &goal_id,
            Some(&run_id),
            Some(&turn_id),
            false,
            "budget_limited",
            &GoalState::BudgetLimited,
            3,
        );
        let recovered = events.goal_recovered(
            &goal_id,
            &run_id,
            Some(&turn_id),
            &GoalState::Paused {
                reason: GoalPauseReason::Recovery,
                message: "recovered stale owner".to_string(),
            },
        );
        let completed = events.goal_completed(
            &goal_id,
            Some(&run_id),
            &intent.evidence,
            &GoalUsage {
                charged_input_tokens: 10,
                output_tokens: 2,
                ..GoalUsage::default()
            },
        );

        assert_eq!(started.event_type, EventType::GoalTurnStarted);
        assert_eq!(started.payload["outer_turn_id"], turn_id.as_str());
        assert_eq!(started.payload["origin"], "continuation");
        assert!(started.event_type.is_semantic());
        assert_eq!(requested.event_type, EventType::GoalIntentRequested);
        assert_eq!(requested.payload["intent_id"], intent.intent_id.as_str());
        assert_eq!(requested.payload["requested_state"], "complete");
        assert_eq!(admission.event_type, EventType::GoalContinuationRejected);
        assert_eq!(admission.payload["rejection_code"], "budget_limited");
        assert_eq!(admission.payload["goal_run_id"], run_id.as_str());
        assert_eq!(admission.payload["outer_turn_id"], turn_id.as_str());
        assert_eq!(
            admission.payload["current_state"]["status"],
            "budget_limited"
        );
        assert_eq!(admission.payload["counters"]["continuation_count"], 3);
        assert!(admission.event_type.is_semantic());
        assert_eq!(recovered.event_type, EventType::GoalRecovered);
        assert_eq!(recovered.payload["stale_goal_run_id"], run_id.as_str());
        assert_eq!(recovered.payload["outer_turn_id"], turn_id.as_str());
        assert!(recovered.event_type.is_semantic());
        assert_eq!(completed.event_type, EventType::GoalCompleted);
        assert_eq!(
            completed.payload["evidence"][0]["summary"],
            "focused tests passed"
        );
        assert_eq!(completed.payload["usage"]["charged_input_tokens"], 10);
        assert!(completed.event_type.is_semantic());
    }

    #[test]
    fn event_envelope_serializes_to_valid_json() {
        let mut f = EventFactory::new("run-1".to_string());
        let identity = ModelResponseIdentity::new(TurnId::new());
        let mut output = Vec::new();
        EventSink::new(&mut output, OutputFormat::Jsonl)
            .emit(f.assistant_message_delta(&identity, "test text"))
            .unwrap();
        let json = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "assistant.message.delta");
        assert_eq!(parsed["payload"]["text"], "test text");
        assert_eq!(parsed["seq"], 0);
        assert_eq!(parsed["version"], "1");
    }

    #[test]
    fn forked_event_factories_share_one_sequence() {
        let mut foreground = EventFactory::new("thread-1".to_string());
        let mut background = foreground.fork();

        let events = publish([
            foreground.error("foreground"),
            background.error("background"),
            foreground.error("foreground again"),
        ]);

        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(events[0].run_id, events[1].run_id);
        assert_eq!(events[1].run_id, events[2].run_id);
    }
}
