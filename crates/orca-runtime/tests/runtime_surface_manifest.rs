use std::collections::BTreeSet;

use orca_core::event_schema::EventType;

// Exact manifest bytes are the reviewed runtime-surface contract fixture.
const MANIFEST: &str = include_str!(
    "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
);

macro_rules! event_inventory {
    ($($variant:ident),+ $(,)?) => {
        const CURRENT_EVENT_TYPES: &[EventType] = &[$(EventType::$variant),+];

        fn event_variant_name(event_type: EventType) -> &'static str {
            match event_type {
                $(EventType::$variant => stringify!($variant)),+
            }
        }
    };
}

event_inventory!(
    SessionStarted,
    TurnStarted,
    AssistantReasoningDelta,
    AssistantMessageDelta,
    ModelResponseCompleted,
    ProviderReplayUpdated,
    UsageUpdated,
    ContextUpdated,
    ContextCompactionStarted,
    ContextCompacted,
    ModelRouted,
    ApprovalRequested,
    ApprovalResolved,
    ToolCallProgress,
    ToolOutputDelta,
    ToolCallRequested,
    ToolCallCompleted,
    PlanUpdated,
    GoalCreated,
    GoalRunStarted,
    GoalTurnStarted,
    GoalIntentRequested,
    GoalIntentAcknowledged,
    GoalTurnFinished,
    GoalVerificationCompleted,
    GoalTransitioned,
    GoalContinuationAdmitted,
    GoalContinuationRejected,
    GoalPaused,
    GoalRecovered,
    GoalCompleted,
    SubagentStarted,
    SubagentProgress,
    SubagentCompleted,
    AgentContinuationCheckpointed,
    AgentContinuationSuspended,
    AgentContinuationResumed,
    AgentContinuationOrphanReconciled,
    AgentContinuationIndeterminate,
    WorkflowStarted,
    WorkflowResumed,
    WorkflowPhaseStarted,
    WorkflowPhaseCompleted,
    WorkflowAgentStarted,
    WorkflowAgentCached,
    WorkflowAgentCompleted,
    WorkflowAgentFailed,
    WorkflowPaused,
    WorkflowStopped,
    WorkflowCompleted,
    WorkflowFailed,
    WorkflowResultAvailable,
    WorkflowTasksUpdated,
    TaskStatusUpdated,
    VerificationStarted,
    VerificationCompleted,
    Error,
    SessionCompleted,
);

#[test]
fn manifest_source_facts_exactly_match_the_typed_event_inventory() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let declared = manifest["source_facts"]
        .as_array()
        .expect("source_facts")
        .iter()
        .map(|row| row[0].as_str().expect("source fact id"))
        .collect::<Vec<_>>();
    let current = CURRENT_EVENT_TYPES
        .iter()
        .copied()
        .map(event_variant_name)
        .collect::<Vec<_>>();

    assert_eq!(declared.len(), 58, "the reviewed baseline has 58 events");
    assert_eq!(
        declared.iter().collect::<BTreeSet<_>>().len(),
        declared.len(),
        "source fact ids must be unique"
    );
    assert_eq!(
        declared, current,
        "EventType drift requires contract review"
    );
}
