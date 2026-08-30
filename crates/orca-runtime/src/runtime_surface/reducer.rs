use super::commands::{
    MutationCommitAck, SURFACE_COMMIT_BATCH_BYTE_LIMIT, SURFACE_COMMIT_BATCH_EVENT_LIMIT,
    SurfaceCommitBatch, SurfaceCommitBatchPreflightErrorCode, SurfaceCommitBatchPreflightResult,
    SurfaceEvent, SurfaceEventEnvelope, SurfaceSnapshot,
};
use super::identity::{
    ByteCount, ByteOffset, CanonicalBackgroundFenceV1, CanonicalSurfaceScopeV1, CommitClass,
    ContextWindowId, CursorSourceRevision, DisplayText, InteractionRevision, NonEmptyText,
    PinnedContextSourceRevision, ResponseRouteEpoch, SafeDiagnosticText, SequenceNumber,
    Sha256Digest, SurfaceCommitId, SurfaceCursor, SurfaceEventId, SurfaceFinalizeIntentId,
    SurfaceGenerationId, SurfaceGoalId, SurfaceGoalIntentId, SurfaceGoalOuterTurnId,
    SurfaceGoalRunId, SurfaceInteractionId, SurfaceItemId, SurfaceOperationFence,
    SurfaceOperationId, SurfaceRequestId, SurfaceScope, SurfaceSettlementId, SurfaceSubagentId,
    SurfaceTaskFence, SurfaceTaskId, SurfaceToolCallId, SurfaceTurnId, SurfaceWorkflowFence,
    SurfaceWorkflowRunId, TaskRevision, ThreadOwnerEpoch, UnixMillis, UuidV7, WorkflowRevision,
    canonical_background_fence_v1, canonical_surface_scope_v1,
};
use super::interaction::{
    AuthorityFingerprint, CanonicalInteractionPatchV1,
    DurableInteractionContinuationOperationIdentity, InteractionCancelReason,
    InteractionExpiryAuthorityFailure, InteractionPatch, InteractionUnavailableDisposition,
    SurfaceInteractionKind, SurfaceInteractionLifecycle, SurfaceInteractionRequest,
    SurfaceInteractionResolutionReceipt, SurfaceInteractionRoute, SurfaceInteractionSafeProjection,
    SurfaceInteractionView, SurfaceToolAction, SurfaceToolRequest, canonical_interaction_patch_v1,
};
use super::operation::{
    AdmissionRejectionReason, AdmittedInput, CancelReason, FailureClass, FinalizationDegradedCause,
    FinalizationStartedAtCursor, GenerationAttempt, GenerationCompletionStatus,
    GenerationExecutionFailureClass, GenerationInputState, GenerationPhase, GenerationRecord,
    GenerationStartedWitness, GenerationStopReason, GoalOuterTurnOrigin, InputResolutionErrorCode,
    LiveOperationCapsule, NonReplayableReason, NotAdmittedReason, NotStartedReason,
    OperationBudget, OperationFinalizationCause, OperationFinalizationRecord, OperationKind,
    OperationPhase, OperationRecord, OperationTerminal, OperationTerminalRecord,
    PendingControlIntent, Replayability, ReservationFinalizerReason, SurfaceAgentLoopTurn,
    SurfaceGoalGenerationIdentity, SurfaceResolvedInputFact, SurfaceSettlementReceipt,
    SurfaceShutdownReason, SuspendedFinalizationCause, SuspensionCause, TerminalizationCause,
    TurnRequestBudgetScope, UsageTotals,
};
use super::projection::{
    AssistantChannel, AssistantPatch, CapabilityCallResult, CompactionReason, CompactionState,
    ExternalEffectKind, GoalContinuationDecision, GoalContinuationStopReason, GoalPatch,
    GoalPatchEnvelope, GoalUsage, ItemPatch, ItemRemovalReason, McpCatalogPatch, OperationPatch,
    PinnedContextPatch, SessionPatch, SettingsPatch, SubagentPatch, SurfaceAssistantStreamState,
    SurfaceBackgroundOperation, SurfaceCapabilityCall, SurfaceCapabilityCallKind,
    SurfaceCapabilityCallState, SurfaceCompletedModelResponse, SurfaceCompletionVerification,
    SurfaceContextSnapshot, SurfaceFactFamily, SurfaceGoal, SurfaceGoalCloseReason,
    SurfaceGoalClosedRunReceipt, SurfaceGoalIntent, SurfaceGoalIntentAck,
    SurfaceGoalOuterTurnReceipt, SurfaceGoalOuterTurnReceiptOrigin, SurfaceGoalPauseReason,
    SurfaceGoalReceiptState, SurfaceGoalRun, SurfaceGoalRunOrigin, SurfaceGoalRunPhase,
    SurfaceGoalState, SurfaceGoalStoreReceipt, SurfaceHealthIssue, SurfaceHealthIssueId,
    SurfaceItem, SurfaceItemOrigin, SurfacePinnedContextEntry, SurfacePinnedContextKind,
    SurfacePlanSnapshot, SurfaceRemoteTerminalLease, SurfaceRemoteTerminalLeaseState,
    SurfaceSubagentOwner, SurfaceSubagentSource, SurfaceSubagentStatus,
    SurfaceSubagentTerminalStatus, SurfaceTask, SurfaceTaskStatus, SurfaceTaskType,
    SurfaceToolResultKind, SurfaceToolView, SurfaceToolViewState, SurfaceUsageSnapshot,
    SurfaceUserInputState, SurfaceVerificationResult, SurfaceWorkflow, SurfaceWorkflowAgent,
    SurfaceWorkflowAgentStatus, SurfaceWorkflowStatus, TaskPatch, ToolInvocationStarted, ToolPatch,
    WorkflowPatch,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[derive(Serialize)]
enum CanonicalOperationPatchV1<'a> {
    Requested {
        operation: &'a OperationRecord,
    },
    ReservationQueueChanged {
        operation_id: &'a SurfaceOperationId,
        reservation_sequence: SequenceNumber,
        ready_for_admission: bool,
        queue_position: u32,
    },
    Admitted {
        operation_id: &'a SurfaceOperationId,
        logical_turn_id: &'a SurfaceTurnId,
        input: &'a AdmittedInput,
        first_generation: &'a GenerationRecord,
    },
    InputBindingsResolved {
        fence: &'a SurfaceOperationFence,
        input_item_id: &'a SurfaceItemId,
        fact: &'a SurfaceResolvedInputFact,
    },
    InputBindingsFailed {
        fence: &'a SurfaceOperationFence,
        input_item_id: &'a SurfaceItemId,
        code: InputResolutionErrorCode,
        message: &'a SafeDiagnosticText,
    },
    ControlIntentCommitted {
        operation_id: &'a SurfaceOperationId,
        request_id: &'a SurfaceRequestId,
        intent: &'a PendingControlIntent,
    },
    GenerationReserved {
        generation: &'a GenerationRecord,
    },
    GenerationStarted {
        fence: &'a SurfaceOperationFence,
        witness: &'a GenerationStartedWitness,
    },
    AgentLoopTurnStarted {
        turn: &'a SurfaceAgentLoopTurn,
    },
    ModelRouteSelected {
        fence: &'a SurfaceOperationFence,
        requested_model: &'a NonEmptyText,
        actual_model: &'a NonEmptyText,
        reason: &'a NonEmptyText,
    },
    VerificationStarted {
        fence: &'a SurfaceOperationFence,
        verification_id: &'a UuidV7,
        command: &'a NonEmptyText,
    },
    VerificationCompleted {
        fence: &'a SurfaceOperationFence,
        verification_id: &'a UuidV7,
        result: &'a SurfaceVerificationResult,
    },
    GenerationStopped {
        fence: &'a SurfaceOperationFence,
        reason: &'a GenerationStopReason,
        usage_delta: &'a UsageTotals,
    },
    GenerationTransferred {
        fence: &'a SurfaceOperationFence,
        background_fence: CanonicalBackgroundFenceV1<'a>,
        task_id: &'a Option<SurfaceTaskId>,
    },
    Suspended {
        operation_id: &'a SurfaceOperationId,
        cause: &'a SuspensionCause,
    },
    SuspensionRebasedAfterUnstartedResume {
        operation_id: &'a SurfaceOperationId,
        previous_cause: &'a SuspensionCause,
        replacement_fence: &'a SurfaceOperationFence,
        rebased_cause: &'a SuspensionCause,
    },
    RecoveryRequired {
        operation_id: &'a SurfaceOperationId,
        last_generation: SurfaceGenerationId,
    },
    FinalizationStarted {
        operation_id: &'a SurfaceOperationId,
        finalize_intent_id: &'a SurfaceFinalizeIntentId,
        terminal_commit_id: &'a SurfaceCommitId,
        selected_cause: &'a OperationFinalizationCause,
        suspended_cause: &'a Option<SuspendedFinalizationCause>,
        expected_settlements: &'a Vec<SurfaceSettlementId>,
    },
    FinalizationSettlementRecorded {
        operation_id: &'a SurfaceOperationId,
        finalize_intent_id: &'a SurfaceFinalizeIntentId,
        receipt: &'a SurfaceSettlementReceipt,
    },
    FinalizationDegraded {
        operation_id: &'a SurfaceOperationId,
        finalize_intent_id: &'a SurfaceFinalizeIntentId,
        cause: &'a FinalizationDegradedCause,
        last_error: &'a DisplayText,
    },
    Terminal {
        record: &'a OperationTerminalRecord,
    },
}

fn canonical_operation_patch_v1(patch: &OperationPatch) -> CanonicalOperationPatchV1<'_> {
    match patch {
        OperationPatch::Requested { operation } => {
            CanonicalOperationPatchV1::Requested { operation }
        }
        OperationPatch::ReservationQueueChanged {
            operation_id,
            reservation_sequence,
            ready_for_admission,
            queue_position,
        } => CanonicalOperationPatchV1::ReservationQueueChanged {
            operation_id,
            reservation_sequence: *reservation_sequence,
            ready_for_admission: *ready_for_admission,
            queue_position: *queue_position,
        },
        OperationPatch::Admitted {
            operation_id,
            logical_turn_id,
            input,
            first_generation,
        } => CanonicalOperationPatchV1::Admitted {
            operation_id,
            logical_turn_id,
            input,
            first_generation,
        },
        OperationPatch::InputBindingsResolved {
            fence,
            input_item_id,
            fact,
        } => CanonicalOperationPatchV1::InputBindingsResolved {
            fence,
            input_item_id,
            fact,
        },
        OperationPatch::InputBindingsFailed {
            fence,
            input_item_id,
            code,
            message,
        } => CanonicalOperationPatchV1::InputBindingsFailed {
            fence,
            input_item_id,
            code: *code,
            message,
        },
        OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent,
        } => CanonicalOperationPatchV1::ControlIntentCommitted {
            operation_id,
            request_id,
            intent,
        },
        OperationPatch::GenerationReserved { generation } => {
            CanonicalOperationPatchV1::GenerationReserved { generation }
        }
        OperationPatch::GenerationStarted { fence, witness } => {
            CanonicalOperationPatchV1::GenerationStarted { fence, witness }
        }
        OperationPatch::AgentLoopTurnStarted { turn } => {
            CanonicalOperationPatchV1::AgentLoopTurnStarted { turn }
        }
        OperationPatch::ModelRouteSelected {
            fence,
            requested_model,
            actual_model,
            reason,
        } => CanonicalOperationPatchV1::ModelRouteSelected {
            fence,
            requested_model,
            actual_model,
            reason,
        },
        OperationPatch::VerificationStarted {
            fence,
            verification_id,
            command,
        } => CanonicalOperationPatchV1::VerificationStarted {
            fence,
            verification_id,
            command,
        },
        OperationPatch::VerificationCompleted {
            fence,
            verification_id,
            result,
        } => CanonicalOperationPatchV1::VerificationCompleted {
            fence,
            verification_id,
            result,
        },
        OperationPatch::GenerationStopped {
            fence,
            reason,
            usage_delta,
        } => CanonicalOperationPatchV1::GenerationStopped {
            fence,
            reason,
            usage_delta,
        },
        OperationPatch::GenerationTransferred {
            fence,
            background_fence,
            task_id,
        } => CanonicalOperationPatchV1::GenerationTransferred {
            fence,
            background_fence: canonical_background_fence_v1(background_fence),
            task_id,
        },
        OperationPatch::Suspended {
            operation_id,
            cause,
        } => CanonicalOperationPatchV1::Suspended {
            operation_id,
            cause,
        },
        OperationPatch::SuspensionRebasedAfterUnstartedResume {
            operation_id,
            previous_cause,
            replacement_fence,
            rebased_cause,
        } => CanonicalOperationPatchV1::SuspensionRebasedAfterUnstartedResume {
            operation_id,
            previous_cause,
            replacement_fence,
            rebased_cause,
        },
        OperationPatch::RecoveryRequired {
            operation_id,
            last_generation,
        } => CanonicalOperationPatchV1::RecoveryRequired {
            operation_id,
            last_generation: *last_generation,
        },
        OperationPatch::FinalizationStarted {
            operation_id,
            finalize_intent_id,
            terminal_commit_id,
            selected_cause,
            suspended_cause,
            expected_settlements,
        } => CanonicalOperationPatchV1::FinalizationStarted {
            operation_id,
            finalize_intent_id,
            terminal_commit_id,
            selected_cause,
            suspended_cause,
            expected_settlements,
        },
        OperationPatch::FinalizationSettlementRecorded {
            operation_id,
            finalize_intent_id,
            receipt,
        } => CanonicalOperationPatchV1::FinalizationSettlementRecorded {
            operation_id,
            finalize_intent_id,
            receipt,
        },
        OperationPatch::FinalizationDegraded {
            operation_id,
            finalize_intent_id,
            cause,
            last_error,
        } => CanonicalOperationPatchV1::FinalizationDegraded {
            operation_id,
            finalize_intent_id,
            cause,
            last_error,
        },
        OperationPatch::Terminal { record } => CanonicalOperationPatchV1::Terminal { record },
    }
}

#[derive(Serialize)]
struct CanonicalTaskV1<'a> {
    task_id: &'a SurfaceTaskId,
    revision: TaskRevision,
    task_type: SurfaceTaskType,
    status: SurfaceTaskStatus,
    backgrounded: bool,
    description: &'a DisplayText,
    created_at: UnixMillis,
    started_at: &'a Option<UnixMillis>,
    completed_at: &'a Option<UnixMillis>,
    parent_operation: &'a Option<SurfaceOperationId>,
    parent_task_id: &'a Option<SurfaceTaskId>,
    background_fence: Option<CanonicalBackgroundFenceV1<'a>>,
    workflow_run_id: &'a Option<SurfaceWorkflowRunId>,
    subagent_id: &'a Option<SurfaceSubagentId>,
    pending_interaction_id: &'a Option<SurfaceInteractionId>,
    usage: &'a Option<UsageTotals>,
    result: &'a Option<DisplayText>,
    error: &'a Option<DisplayText>,
}

fn canonical_task_v1(task: &SurfaceTask) -> CanonicalTaskV1<'_> {
    CanonicalTaskV1 {
        task_id: &task.task_id,
        revision: task.revision,
        task_type: task.task_type,
        status: task.status,
        backgrounded: task.backgrounded,
        description: &task.description,
        created_at: task.created_at,
        started_at: &task.started_at,
        completed_at: &task.completed_at,
        parent_operation: &task.parent_operation,
        parent_task_id: &task.parent_task_id,
        background_fence: task
            .background_fence
            .as_ref()
            .map(canonical_background_fence_v1),
        workflow_run_id: &task.workflow_run_id,
        subagent_id: &task.subagent_id,
        pending_interaction_id: &task.pending_interaction_id,
        usage: &task.usage,
        result: &task.result,
        error: &task.error,
    }
}

#[derive(Serialize)]
enum CanonicalTaskPatchV1<'a> {
    Upserted {
        expected_revision: &'a Option<TaskRevision>,
        task: CanonicalTaskV1<'a>,
    },
    StatusChanged {
        task_id: &'a SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        status: SurfaceTaskStatus,
        completed_at: &'a Option<UnixMillis>,
        result: &'a Option<DisplayText>,
        error: &'a Option<DisplayText>,
    },
    InteractionChanged {
        task_id: &'a SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        status: SurfaceTaskStatus,
        pending_interaction_id: &'a Option<SurfaceInteractionId>,
    },
    OwnershipChanged {
        task_id: &'a SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        backgrounded: bool,
        background_fence: Option<CanonicalBackgroundFenceV1<'a>>,
    },
    Reconciled {
        source_revision: TaskRevision,
        tasks: Vec<CanonicalTaskV1<'a>>,
    },
}

fn canonical_task_patch_v1(patch: &TaskPatch) -> CanonicalTaskPatchV1<'_> {
    match patch {
        TaskPatch::Upserted {
            expected_revision,
            task,
        } => CanonicalTaskPatchV1::Upserted {
            expected_revision,
            task: canonical_task_v1(task),
        },
        TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status,
            completed_at,
            result,
            error,
        } => CanonicalTaskPatchV1::StatusChanged {
            task_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            status: *status,
            completed_at,
            result,
            error,
        },
        TaskPatch::InteractionChanged {
            task_id,
            expected_revision,
            next_revision,
            status,
            pending_interaction_id,
        } => CanonicalTaskPatchV1::InteractionChanged {
            task_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            status: *status,
            pending_interaction_id,
        },
        TaskPatch::OwnershipChanged {
            task_id,
            expected_revision,
            next_revision,
            backgrounded,
            background_fence,
        } => CanonicalTaskPatchV1::OwnershipChanged {
            task_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            backgrounded: *backgrounded,
            background_fence: background_fence.as_ref().map(canonical_background_fence_v1),
        },
        TaskPatch::Reconciled {
            source_revision,
            tasks,
        } => CanonicalTaskPatchV1::Reconciled {
            source_revision: *source_revision,
            tasks: tasks.iter().map(canonical_task_v1).collect(),
        },
    }
}

#[derive(Serialize)]
enum CanonicalEventV1<'a> {
    Operation(CanonicalOperationPatchV1<'a>),
    Item(&'a ItemPatch),
    Assistant(&'a AssistantPatch),
    Tool(&'a ToolPatch),
    Plan(&'a SurfacePlanSnapshot),
    Usage(&'a SurfaceUsageSnapshot),
    Context(&'a SurfaceContextSnapshot),
    Interaction(CanonicalInteractionPatchV1<'a>),
    Task(CanonicalTaskPatchV1<'a>),
    Workflow(&'a WorkflowPatch),
    Subagent(&'a SubagentPatch),
    Goal(&'a GoalPatchEnvelope),
    Settings(&'a SettingsPatch),
    McpCatalog(&'a McpCatalogPatch),
    PinnedContext(&'a PinnedContextPatch),
    Session(&'a SessionPatch),
}

fn canonical_event_v1(event: &SurfaceEvent) -> CanonicalEventV1<'_> {
    match event {
        SurfaceEvent::Operation(patch) => {
            CanonicalEventV1::Operation(canonical_operation_patch_v1(patch))
        }
        SurfaceEvent::Item(patch) => CanonicalEventV1::Item(patch),
        SurfaceEvent::Assistant(patch) => CanonicalEventV1::Assistant(patch),
        SurfaceEvent::Tool(patch) => CanonicalEventV1::Tool(patch),
        SurfaceEvent::Plan(snapshot) => CanonicalEventV1::Plan(snapshot),
        SurfaceEvent::Usage(snapshot) => CanonicalEventV1::Usage(snapshot),
        SurfaceEvent::Context(snapshot) => CanonicalEventV1::Context(snapshot),
        SurfaceEvent::Interaction(patch) => {
            CanonicalEventV1::Interaction(canonical_interaction_patch_v1(patch))
        }
        SurfaceEvent::Task(patch) => CanonicalEventV1::Task(canonical_task_patch_v1(patch)),
        SurfaceEvent::Workflow(patch) => CanonicalEventV1::Workflow(patch),
        SurfaceEvent::Subagent(patch) => CanonicalEventV1::Subagent(patch),
        SurfaceEvent::Goal(patch) => CanonicalEventV1::Goal(patch),
        SurfaceEvent::Settings(patch) => CanonicalEventV1::Settings(patch),
        SurfaceEvent::McpCatalog(patch) => CanonicalEventV1::McpCatalog(patch),
        SurfaceEvent::PinnedContext(patch) => CanonicalEventV1::PinnedContext(patch),
        SurfaceEvent::Session(patch) => CanonicalEventV1::Session(patch),
    }
}

#[derive(Serialize)]
struct CanonicalEventDigestV1<'a> {
    version: u8,
    scope: CanonicalSurfaceScopeV1<'a>,
    event: CanonicalEventV1<'a>,
}

#[derive(Serialize)]
struct CanonicalEnvelopeV1<'a> {
    ordinal: u32,
    event_id: &'a SurfaceEventId,
    commit_class: &'a CommitClass,
    scope: CanonicalSurfaceScopeV1<'a>,
    event: CanonicalEventV1<'a>,
}

fn sha256(value: &[u8]) -> Sha256Digest {
    let digest: [u8; 32] = Sha256::digest(value).into();
    Sha256Digest::new(digest)
}

#[derive(Serialize)]
enum CanonicalDurableReplayabilityV1<'a> {
    Replayable { capsule_digest: &'a Sha256Digest },
    NonReplayable { reason: NonReplayableReason },
}

#[derive(Serialize)]
struct CanonicalReplayabilityDigestV1<'a> {
    version: u8,
    replayability: CanonicalDurableReplayabilityV1<'a>,
}

pub fn canonical_replayability_digest(replayability: &Replayability) -> Sha256Digest {
    let replayability = match replayability {
        Replayability::Replayable { capsule_digest, .. } => {
            CanonicalDurableReplayabilityV1::Replayable { capsule_digest }
        }
        Replayability::NonReplayable { reason, .. } => {
            CanonicalDurableReplayabilityV1::NonReplayable { reason: *reason }
        }
    };
    sha256(
        &serde_json::to_vec(&CanonicalReplayabilityDigestV1 {
            version: 1,
            replayability,
        })
        .expect("canonical replayability is serializable"),
    )
}

pub fn canonical_event_digest(envelope: &SurfaceEventEnvelope) -> Sha256Digest {
    let canonical = CanonicalEventDigestV1 {
        version: 1,
        scope: canonical_surface_scope_v1(&envelope.scope),
        event: canonical_event_v1(&envelope.event),
    };
    sha256(&serde_json::to_vec(&canonical).expect("canonical surface event is serializable"))
}

#[derive(Serialize)]
struct CanonicalBatchDigestV1<'a> {
    version: u8,
    event_digests: &'a [Sha256Digest],
    event_count: u32,
    commit_class: &'a CommitClass,
}

pub fn canonical_batch_digest(batch: &SurfaceCommitBatch) -> Sha256Digest {
    let event_digests = batch
        .events
        .as_slice()
        .iter()
        .map(canonical_event_digest)
        .collect::<Vec<_>>();
    let canonical = CanonicalBatchDigestV1 {
        version: 1,
        event_digests: &event_digests,
        event_count: batch.event_count,
        commit_class: &batch.commit_class,
    };
    sha256(&serde_json::to_vec(&canonical).expect("canonical batch digest is serializable"))
}

#[derive(Serialize)]
struct CanonicalBatchV1<'a> {
    version: u8,
    cursor_before: &'a SurfaceCursor,
    cursor_after: &'a SurfaceCursor,
    commit_class: &'a CommitClass,
    event_count: u32,
    batch_digest: &'a Sha256Digest,
    events: Vec<CanonicalEnvelopeV1<'a>>,
}

fn canonical_batch_bytes(batch: &SurfaceCommitBatch) -> Vec<u8> {
    let events = batch
        .events
        .as_slice()
        .iter()
        .map(|envelope| CanonicalEnvelopeV1 {
            ordinal: envelope.ordinal,
            event_id: &envelope.event_id,
            commit_class: &envelope.commit_class,
            scope: canonical_surface_scope_v1(&envelope.scope),
            event: canonical_event_v1(&envelope.event),
        })
        .collect();
    serde_json::to_vec(&CanonicalBatchV1 {
        version: 1,
        cursor_before: &batch.cursor_before,
        cursor_after: &batch.cursor_after,
        commit_class: &batch.commit_class,
        event_count: batch.event_count,
        batch_digest: &batch.batch_digest,
        events,
    })
    .expect("canonical surface batch is serializable")
}

pub fn canonical_batch_encoded_bytes(batch: &SurfaceCommitBatch) -> u64 {
    canonical_batch_bytes(batch).len() as u64
}

#[derive(Clone, PartialEq)]
struct AppliedTransitionRecord {
    event_id: SurfaceEventId,
    commit_id: SurfaceCommitId,
    event_digest: Sha256Digest,
    ordinal: u32,
    batch_cursor_after: SurfaceCursor,
}

#[derive(Clone, PartialEq)]
struct AppliedBatchRecord {
    commit_class: CommitClass,
    event_count: u32,
    batch_digest: Sha256Digest,
    cursor_before: SurfaceCursor,
    cursor_after: SurfaceCursor,
    ordered_events: Vec<(SurfaceEventId, Sha256Digest)>,
}

#[derive(Clone, PartialEq)]
struct AppliedControlIntentRecord {
    operation_id: SurfaceOperationId,
    intent: PendingControlIntent,
    event_id: SurfaceEventId,
    cursor: SurfaceCursor,
    commit_class: CommitClass,
}

#[derive(Clone, PartialEq)]
struct AppliedGoalRecoveryRecord {
    goal_id: SurfaceGoalId,
    stale_run: SurfaceGoalClosedRunReceipt,
    goal_receipt_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
struct AppliedGoalSuccessorAuthorization {
    predecessor: SurfaceOperationFence,
}

#[derive(Clone, PartialEq)]
struct AppliedFinalizationDegradedProof {
    finalize_intent_id: SurfaceFinalizeIntentId,
    selected_cause: OperationFinalizationCause,
    cause: FinalizationDegradedCause,
}

#[derive(Clone, PartialEq)]
struct SessionCloseWitness {
    reason: SurfaceShutdownReason,
    barrier_id: SurfaceSettlementId,
    closing_commit_id: SurfaceCommitId,
    plan_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
struct LiveToolArgumentProgress {
    fence: SurfaceOperationFence,
    arguments_bytes: ByteCount,
}

const APPLIED_HISTORY_SHARD_COUNT: usize = 64;

#[derive(Clone, PartialEq)]
struct CowShardedBTreeMap<K, V> {
    shards: Vec<Arc<BTreeMap<K, V>>>,
}

impl<K, V> Default for CowShardedBTreeMap<K, V>
where
    K: Ord,
{
    fn default() -> Self {
        Self {
            shards: vec![Arc::new(BTreeMap::new()); APPLIED_HISTORY_SHARD_COUNT],
        }
    }
}

impl<K, V> CowShardedBTreeMap<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone,
{
    fn shard_index(key: &K) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish() as usize % APPLIED_HISTORY_SHARD_COUNT
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.shards[Self::shard_index(key)].get(key)
    }

    fn insert(&mut self, key: K, value: V) -> Option<V> {
        let shard = Self::shard_index(&key);
        Arc::make_mut(&mut self.shards[shard]).insert(key, value)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let shard = Self::shard_index(key);
        Arc::make_mut(&mut self.shards[shard]).remove(key)
    }

    fn any_key(&self, predicate: impl Fn(&K) -> bool) -> bool {
        self.shards.iter().any(|shard| shard.keys().any(&predicate))
    }

    fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| shard.is_empty())
    }

    #[cfg(test)]
    fn shared_shard_count(&self, other: &Self) -> usize {
        self.shards
            .iter()
            .zip(&other.shards)
            .filter(|(left, right)| Arc::ptr_eq(left, right))
            .count()
    }
}

#[derive(Clone, PartialEq)]
pub struct SurfaceReducerState {
    snapshot: SurfaceSnapshot,
    applied: CowShardedBTreeMap<(SurfaceEventId, SurfaceCommitId), AppliedTransitionRecord>,
    applied_batches: CowShardedBTreeMap<SurfaceCommitId, AppliedBatchRecord>,
    applied_control_intents: Vec<AppliedControlIntentRecord>,
    applied_goal_receipts: HashSet<(SurfaceCommitId, Sha256Digest)>,
    goal_recoveries: Vec<AppliedGoalRecoveryRecord>,
    goal_successor_authorizations: Vec<AppliedGoalSuccessorAuthorization>,
    degraded_finalizations: BTreeMap<SurfaceOperationId, AppliedFinalizationDegradedProof>,
    session_close: Option<SessionCloseWitness>,
    live_tool_argument_progress: BTreeMap<SurfaceToolCallId, LiveToolArgumentProgress>,
}

impl SurfaceReducerState {
    pub fn new(snapshot: SurfaceSnapshot) -> Self {
        Self {
            snapshot,
            applied: CowShardedBTreeMap::default(),
            applied_batches: CowShardedBTreeMap::default(),
            applied_control_intents: Vec::new(),
            applied_goal_receipts: HashSet::new(),
            goal_recoveries: Vec::new(),
            goal_successor_authorizations: Vec::new(),
            degraded_finalizations: BTreeMap::new(),
            session_close: None,
            live_tool_argument_progress: BTreeMap::new(),
        }
    }

    pub fn snapshot(&self) -> &SurfaceSnapshot {
        &self.snapshot
    }

    pub(crate) fn control_intent_acknowledgement(
        &self,
        operation_id: &SurfaceOperationId,
        intent: &PendingControlIntent,
    ) -> Option<MutationCommitAck> {
        self.applied_control_intents
            .iter()
            .rev()
            .find(|record| &record.operation_id == operation_id && &record.intent == intent)
            .map(|record| MutationCommitAck::ThreadLocalCursor {
                cursor: record.cursor.clone(),
                family: SurfaceFactFamily::Operation,
                event_id: record.event_id.clone(),
                commit_class: record.commit_class.clone(),
            })
    }

    pub(crate) fn align_rematerialization_baseline(
        &mut self,
        cursor: SurfaceCursor,
        owner_epoch: ThreadOwnerEpoch,
    ) {
        debug_assert!(self.applied.is_empty());
        debug_assert!(self.applied_batches.is_empty());
        debug_assert_eq!(cursor.next_seq.get(), 0);
        self.snapshot.cursor = cursor;
        self.snapshot.thread.owner_epoch = owner_epoch;
    }

    pub(crate) fn finalization_degraded_cause(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> Option<&FinalizationDegradedCause> {
        self.degraded_finalizations
            .get(operation_id)
            .map(|proof| &proof.cause)
    }

    pub(crate) fn has_goal_store_receipt(
        &self,
        store_commit_id: &SurfaceCommitId,
        receipt_digest: &Sha256Digest,
    ) -> bool {
        self.applied_goal_receipts
            .contains(&(store_commit_id.clone(), receipt_digest.clone()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReduceMode {
    Live,
    Rematerialization,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceReduceResult {
    Applied {
        state: SurfaceReducerState,
    },
    AlreadyApplied {
        cursor: SurfaceCursor,
        commit_id: SurfaceCommitId,
    },
    Rejected {
        error: SurfaceReducerError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReducerErrorCode {
    CursorMismatch,
    ScopeMismatch,
    CommitClassMismatch,
    StaleRevision,
    IllegalTransition,
    MissingIdentity,
    DuplicateTransition,
    InvalidOrdering,
    PartialBatchReplay,
    GoalReceiptMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceReducerErrorLocation {
    Batch {
        commit_id: SurfaceCommitId,
    },
    Event {
        event_id: SurfaceEventId,
        ordinal: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceReducerError {
    pub code: SurfaceReducerErrorCode,
    pub location: SurfaceReducerErrorLocation,
    pub message: DisplayText,
}

pub fn preflight_batch(batch: &SurfaceCommitBatch) -> SurfaceCommitBatchPreflightResult {
    let observed_event_count = batch.events.as_slice().len() as u64;
    let observed_canonical_encoded_bytes = canonical_batch_encoded_bytes(batch);
    if observed_event_count > SURFACE_COMMIT_BATCH_EVENT_LIMIT
        || observed_canonical_encoded_bytes > SURFACE_COMMIT_BATCH_BYTE_LIMIT
    {
        return SurfaceCommitBatchPreflightResult::Rejected {
            code: SurfaceCommitBatchPreflightErrorCode::CommitBatchTooLarge,
            observed_event_count,
            observed_canonical_encoded_bytes,
            event_limit: SURFACE_COMMIT_BATCH_EVENT_LIMIT,
            byte_limit: SURFACE_COMMIT_BATCH_BYTE_LIMIT,
        };
    }
    SurfaceCommitBatchPreflightResult::Ready {
        event_count: observed_event_count as u32,
        canonical_encoded_bytes: observed_canonical_encoded_bytes,
        batch_digest: canonical_batch_digest(batch),
    }
}

fn commit_id(commit_class: &CommitClass) -> &SurfaceCommitId {
    match commit_class {
        CommitClass::Recorded { commit_id, .. } | CommitClass::Ephemeral { commit_id, .. } => {
            commit_id
        }
    }
}

fn batch_error(
    batch: &SurfaceCommitBatch,
    code: SurfaceReducerErrorCode,
    message: &'static str,
) -> SurfaceReduceResult {
    SurfaceReduceResult::Rejected {
        error: SurfaceReducerError {
            code,
            location: SurfaceReducerErrorLocation::Batch {
                commit_id: commit_id(&batch.commit_class).clone(),
            },
            message: DisplayText::new(message),
        },
    }
}

fn event_error(
    envelope: &SurfaceEventEnvelope,
    code: SurfaceReducerErrorCode,
    message: impl Into<String>,
) -> SurfaceReducerError {
    SurfaceReducerError {
        code,
        location: SurfaceReducerErrorLocation::Event {
            event_id: envelope.event_id.clone(),
            ordinal: envelope.ordinal,
        },
        message: DisplayText::new(message),
    }
}

fn duplicate_result(
    mode: SurfaceReduceMode,
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
) -> Option<SurfaceReduceResult> {
    let id = commit_id(&batch.commit_class);
    if let Some(record) = state.applied_batches.get(id) {
        if record.commit_class != batch.commit_class {
            return Some(batch_error(
                batch,
                SurfaceReducerErrorCode::CommitClassMismatch,
                "commit class differs from applied batch",
            ));
        }
        let ordered_events = batch
            .events
            .as_slice()
            .iter()
            .map(|event| (event.event_id.clone(), canonical_event_digest(event)))
            .collect::<Vec<_>>();
        if record.event_count != batch.event_count
            || record.batch_digest != batch.batch_digest
            || record.cursor_before != batch.cursor_before
            || record.cursor_after != batch.cursor_after
            || record.ordered_events != ordered_events
        {
            return Some(batch_error(
                batch,
                SurfaceReducerErrorCode::DuplicateTransition,
                "applied batch identity differs",
            ));
        }
        let complete = ordered_events
            .iter()
            .enumerate()
            .all(|(ordinal, (event_id, digest))| {
                state
                    .applied
                    .get(&(event_id.clone(), id.clone()))
                    .is_some_and(|event| {
                        event.event_id == *event_id
                            && event.commit_id == *id
                            && event.event_digest == *digest
                            && event.ordinal == ordinal as u32
                            && event.batch_cursor_after == batch.cursor_after
                    })
            });
        if !complete {
            return Some(batch_error(
                batch,
                SurfaceReducerErrorCode::PartialBatchReplay,
                "applied batch index is incomplete",
            ));
        }
        return Some(match mode {
            SurfaceReduceMode::Rematerialization => SurfaceReduceResult::AlreadyApplied {
                cursor: record.cursor_after.clone(),
                commit_id: id.clone(),
            },
            SurfaceReduceMode::Live => batch_error(
                batch,
                SurfaceReducerErrorCode::DuplicateTransition,
                "live application cannot repeat an applied batch",
            ),
        });
    }

    if state
        .applied
        .any_key(|(_, applied_commit_id)| applied_commit_id == id)
    {
        return Some(batch_error(
            batch,
            SurfaceReducerErrorCode::PartialBatchReplay,
            "event records exist without an applied batch record",
        ));
    }
    None
}

fn validate_cursor_and_commit(
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
) -> Result<(), SurfaceReduceResult> {
    let before = &batch.cursor_before;
    let after = &batch.cursor_after;
    let cold_owner_takeover = matches!(
        (
            &batch.commit_class,
            batch.events.as_slice(),
        ),
        (
            CommitClass::Recorded {
                thread_owner_epoch,
                ..
            },
            [SurfaceEventEnvelope {
                scope: SurfaceScope::Thread,
                event: SurfaceEvent::Session(SessionPatch::OwnerEpochChanged { previous, next }),
                ..
            }]
        ) if *previous == state.snapshot.thread.owner_epoch
            && previous < next
            && next == thread_owner_epoch
            && after.incarnation != before.incarnation
    );
    if state.snapshot.cursor != *before
        || before.thread_id != state.snapshot.thread.thread_id
        || after.thread_id != before.thread_id
        || (after.incarnation != before.incarnation && !cold_owner_takeover)
        || after.next_seq.get()
            != before
                .next_seq
                .get()
                .checked_add(batch.event_count as u64)
                .unwrap_or(u64::MAX)
    {
        return Err(batch_error(
            batch,
            SurfaceReducerErrorCode::CursorMismatch,
            "batch cursor boundary is not continuous",
        ));
    }
    let source_matches = match (&batch.commit_class, &after.source_revision) {
        (
            CommitClass::Recorded {
                thread_owner_epoch,
                durable_revision,
                ..
            },
            CursorSourceRevision::Recorded {
                durable_revision: cursor_revision,
            },
        ) => {
            (*thread_owner_epoch == state.snapshot.thread.owner_epoch || cold_owner_takeover)
                && durable_revision == cursor_revision
        }
        (
            CommitClass::Ephemeral {
                incarnation,
                live_revision,
                ..
            },
            CursorSourceRevision::Ephemeral {
                live_revision: cursor_revision,
            },
        ) => incarnation == &before.incarnation && live_revision == cursor_revision,
        _ => false,
    };
    if !source_matches {
        return Err(batch_error(
            batch,
            SurfaceReducerErrorCode::StaleRevision,
            "commit source revision is stale",
        ));
    }
    Ok(())
}

fn scope_matches_event(scope: &SurfaceScope, event: &SurfaceEvent) -> bool {
    match event {
        SurfaceEvent::Plan(_)
        | SurfaceEvent::Usage(_)
        | SurfaceEvent::Settings(_)
        | SurfaceEvent::McpCatalog(_)
        | SurfaceEvent::PinnedContext(_)
        | SurfaceEvent::Session(_) => matches!(scope, SurfaceScope::Thread),
        SurfaceEvent::Context(_) => matches!(
            scope,
            SurfaceScope::Thread | SurfaceScope::Generation { .. }
        ),
        SurfaceEvent::Task(patch) => match (scope, patch) {
            (SurfaceScope::Thread, _) => true,
            (
                SurfaceScope::Background { fence: scope_fence },
                TaskPatch::OwnershipChanged {
                    background_fence: Some(patch_fence),
                    ..
                },
            ) => scope_fence == patch_fence,
            _ => false,
        },
        SurfaceEvent::Goal(envelope) => {
            let goal_id = goal_patch_id(&envelope.patch);
            let causative_generation = match &envelope.patch {
                GoalPatch::OuterTurnStarted { identity }
                | GoalPatch::OuterTurnFinished { identity, .. }
                | GoalPatch::VerificationCompleted { identity, .. } => {
                    Some(&identity.operation_fence)
                }
                GoalPatch::ContinuationDecided { predecessor, .. } => {
                    Some(&predecessor.operation_fence)
                }
                _ => None,
            };
            matches!(
                scope,
                SurfaceScope::Goal {
                    goal_id: scoped,
                    causative_generation: scoped_generation,
                } if scoped == goal_id && scoped_generation.as_ref() == causative_generation
            )
        }
        SurfaceEvent::Operation(patch) => match (scope, patch) {
            (
                SurfaceScope::Operation {
                    operation_id: scoped,
                },
                OperationPatch::Requested { operation },
            ) => scoped == &operation.operation_id,
            (
                SurfaceScope::Operation {
                    operation_id: scoped,
                },
                OperationPatch::ReservationQueueChanged { operation_id, .. }
                | OperationPatch::Admitted { operation_id, .. }
                | OperationPatch::ControlIntentCommitted { operation_id, .. }
                | OperationPatch::SuspensionRebasedAfterUnstartedResume { operation_id, .. }
                | OperationPatch::RecoveryRequired { operation_id, .. }
                | OperationPatch::FinalizationStarted { operation_id, .. }
                | OperationPatch::FinalizationSettlementRecorded { operation_id, .. }
                | OperationPatch::FinalizationDegraded { operation_id, .. },
            ) => scoped == operation_id,
            (
                SurfaceScope::Operation {
                    operation_id: scoped,
                },
                OperationPatch::Suspended { operation_id, .. },
            ) => scoped == operation_id,
            (
                SurfaceScope::Background { fence: scoped },
                OperationPatch::Suspended { operation_id, .. },
            ) => &scoped.operation_fence.operation_id == operation_id,
            (
                SurfaceScope::Operation {
                    operation_id: scoped,
                },
                OperationPatch::Terminal { record },
            ) => scoped == &record.operation_id,
            (
                SurfaceScope::Generation { fence: scoped },
                OperationPatch::GenerationReserved { generation },
            ) => scoped == &generation.fence,
            (
                SurfaceScope::Background { fence: scoped },
                OperationPatch::GenerationReserved { generation },
            ) => {
                scoped.operation_fence.operation_id == generation.fence.operation_id
                    && generation.predecessor.as_ref() == Some(&scoped.operation_fence)
            }
            (
                SurfaceScope::Generation { fence: scoped },
                OperationPatch::GenerationStarted { fence, .. }
                | OperationPatch::InputBindingsResolved { fence, .. }
                | OperationPatch::InputBindingsFailed { fence, .. }
                | OperationPatch::ModelRouteSelected { fence, .. }
                | OperationPatch::VerificationStarted { fence, .. }
                | OperationPatch::VerificationCompleted { fence, .. }
                | OperationPatch::GenerationTransferred { fence, .. },
            ) => scoped == fence,
            (
                SurfaceScope::Generation { fence: scoped },
                OperationPatch::AgentLoopTurnStarted { turn },
            ) => scoped == &turn.fence,
            (
                SurfaceScope::Generation { fence: scoped },
                OperationPatch::GenerationStopped { fence, .. },
            ) => scoped == fence,
            (
                SurfaceScope::Background { fence: scoped },
                OperationPatch::GenerationStopped { fence, .. },
            ) => &scoped.operation_fence == fence,
            (
                SurfaceScope::Background { fence: scoped },
                OperationPatch::ControlIntentCommitted { operation_id, .. },
            ) => &scoped.operation_fence.operation_id == operation_id,
            (
                SurfaceScope::Background { fence: scoped },
                OperationPatch::FinalizationStarted { operation_id, .. }
                | OperationPatch::FinalizationSettlementRecorded { operation_id, .. }
                | OperationPatch::FinalizationDegraded { operation_id, .. },
            ) => &scoped.operation_fence.operation_id == operation_id,
            (SurfaceScope::Background { fence: scoped }, OperationPatch::Terminal { record }) => {
                scoped.operation_fence.operation_id == record.operation_id
            }
            _ => false,
        },
        SurfaceEvent::Item(_) | SurfaceEvent::Tool(_) => matches!(
            scope,
            SurfaceScope::Generation { .. } | SurfaceScope::Background { .. }
        ),
        SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => matches!(
            scope,
            SurfaceScope::Generation { fence } if fence == &stream.fence
        ),
        SurfaceEvent::Assistant(_) => matches!(
            scope,
            SurfaceScope::Generation { .. } | SurfaceScope::Background { .. }
        ),
        SurfaceEvent::Interaction(InteractionPatch::Requested { interaction }) => {
            matches!(
                scope,
                SurfaceScope::Generation { fence } if fence == &interaction.fence
            ) || matches!(
                scope,
                SurfaceScope::Background { fence } if fence.operation_fence == interaction.fence
            )
        }
        SurfaceEvent::Interaction(InteractionPatch::Transferred {
            background_fence, ..
        }) => matches!(
            scope,
            SurfaceScope::Background { fence } if fence == background_fence
        ),
        SurfaceEvent::Interaction(
            InteractionPatch::ContinuationDispatchStarted { .. }
            | InteractionPatch::ContinuationDispatchConsumed { .. },
        ) => matches!(scope, SurfaceScope::Thread),
        SurfaceEvent::Interaction(_) => matches!(
            scope,
            SurfaceScope::Generation { .. } | SurfaceScope::Background { .. }
        ),
        SurfaceEvent::Workflow(_) => matches!(scope, SurfaceScope::Thread),
        SurfaceEvent::Subagent(SubagentPatch::Started { subagent, .. }) => {
            match (scope, &subagent.as_subagent().owner) {
                (
                    SurfaceScope::Generation { fence },
                    SurfaceSubagentOwner::Generation { fence: owner },
                ) => fence == owner,
                (SurfaceScope::Thread, SurfaceSubagentOwner::DetachedTask { .. }) => true,
                _ => false,
            }
        }
        SurfaceEvent::Subagent(SubagentPatch::Progress { owner, .. })
        | SurfaceEvent::Subagent(SubagentPatch::Completed { owner, .. }) => match (scope, owner) {
            (
                SurfaceScope::Generation { fence },
                SurfaceSubagentOwner::Generation { fence: owner },
            ) => fence == owner,
            (SurfaceScope::Thread, SurfaceSubagentOwner::DetachedTask { .. }) => true,
            _ => false,
        },
    }
}

fn validate_batch_structure(
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
) -> Result<(), SurfaceReduceResult> {
    let events = batch.events.as_slice();
    if batch.event_count as usize != events.len() {
        return Err(batch_error(
            batch,
            SurfaceReducerErrorCode::InvalidOrdering,
            "event count does not match batch membership",
        ));
    }
    if canonical_batch_digest(batch) != batch.batch_digest {
        return Err(batch_error(
            batch,
            SurfaceReducerErrorCode::InvalidOrdering,
            "batch digest is not canonical",
        ));
    }
    validate_cursor_and_commit(state, batch)?;
    let mut event_ids = HashSet::with_capacity(events.len());
    for (expected_ordinal, envelope) in events.iter().enumerate() {
        if envelope.commit_class != batch.commit_class {
            return Err(SurfaceReduceResult::Rejected {
                error: event_error(
                    envelope,
                    SurfaceReducerErrorCode::CommitClassMismatch,
                    "event commit class differs from batch",
                ),
            });
        }
        if envelope.ordinal != expected_ordinal as u32 {
            return Err(SurfaceReduceResult::Rejected {
                error: event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "event ordinal is not contiguous",
                ),
            });
        }
        if !event_ids.insert(envelope.event_id.clone()) {
            return Err(SurfaceReduceResult::Rejected {
                error: event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "event id repeats in one batch",
                ),
            });
        }
        if state
            .applied
            .any_key(|(event_id, _)| event_id == &envelope.event_id)
        {
            return Err(SurfaceReduceResult::Rejected {
                error: event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "event id was already applied by another commit",
                ),
            });
        }
        if !scope_matches_event(&envelope.scope, &envelope.event) {
            return Err(SurfaceReduceResult::Rejected {
                error: event_error(
                    envelope,
                    SurfaceReducerErrorCode::ScopeMismatch,
                    "event scope does not match payload identity",
                ),
            });
        }
    }
    if manual_compaction_rebuild_batch(batch)
        && !super::commit::manual_compaction_item_rebuild_paired(state.snapshot(), batch)
    {
        return Err(batch_error(
            batch,
            SurfaceReducerErrorCode::InvalidOrdering,
            "manual compaction item rebuild is not exact",
        ));
    }
    Ok(())
}

fn manual_compaction_rebuild_batch(batch: &SurfaceCommitBatch) -> bool {
    batch.events.as_slice().iter().any(|event| {
        matches!(
            &event.event,
            SurfaceEvent::Context(SurfaceContextSnapshot {
                compaction: CompactionState::Completed {
                    reason: CompactionReason::Manual,
                    ..
                },
                ..
            })
        )
    })
}

pub fn reduce_batch(
    mode: SurfaceReduceMode,
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
) -> SurfaceReduceResult {
    if let SurfaceCommitBatchPreflightResult::Rejected { .. } = preflight_batch(batch) {
        return batch_error(
            batch,
            SurfaceReducerErrorCode::InvalidOrdering,
            "oversized batches must be rejected before reduction",
        );
    }
    if let Some(result) = duplicate_result(mode, state, batch) {
        return result;
    }
    if let Err(result) = validate_batch_structure(state, batch) {
        return result;
    }

    let mut candidate = state.clone();
    for envelope in batch.events.as_slice() {
        if let Err(error) = apply_event(&mut candidate, envelope, batch) {
            return SurfaceReduceResult::Rejected { error };
        }
    }
    if let Err(error) = validate_batch_pairings(&candidate, batch) {
        return SurfaceReduceResult::Rejected { error };
    }
    candidate.snapshot.cursor = batch.cursor_after.clone();

    let id = commit_id(&batch.commit_class).clone();
    let ordered_events = batch
        .events
        .as_slice()
        .iter()
        .map(|envelope| {
            let event_digest = canonical_event_digest(envelope);
            candidate.applied.insert(
                (envelope.event_id.clone(), id.clone()),
                AppliedTransitionRecord {
                    event_id: envelope.event_id.clone(),
                    commit_id: id.clone(),
                    event_digest: event_digest.clone(),
                    ordinal: envelope.ordinal,
                    batch_cursor_after: batch.cursor_after.clone(),
                },
            );
            (envelope.event_id.clone(), event_digest)
        })
        .collect();
    candidate.applied_batches.insert(
        id,
        AppliedBatchRecord {
            commit_class: batch.commit_class.clone(),
            event_count: batch.event_count,
            batch_digest: batch.batch_digest.clone(),
            cursor_before: batch.cursor_before.clone(),
            cursor_after: batch.cursor_after.clone(),
            ordered_events,
        },
    );
    SurfaceReduceResult::Applied { state: candidate }
}

fn apply_event(
    state: &mut SurfaceReducerState,
    envelope: &SurfaceEventEnvelope,
    batch: &SurfaceCommitBatch,
) -> Result<(), SurfaceReducerError> {
    match &envelope.event {
        SurfaceEvent::Operation(patch) => apply_operation_patch(state, envelope, batch, patch),
        SurfaceEvent::Item(patch) => apply_item_patch(&mut state.snapshot, envelope, batch, patch),
        SurfaceEvent::Assistant(patch) => {
            apply_assistant_patch(&mut state.snapshot, envelope, batch, patch)
        }
        SurfaceEvent::Tool(patch) => apply_tool_patch(state, envelope, batch, patch),
        SurfaceEvent::Plan(plan) => {
            if state.snapshot.plan.revision.get().checked_add(1) != Some(plan.revision.get()) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "plan revision is not contiguous",
                ));
            }
            state.snapshot.plan = plan.clone();
            Ok(())
        }
        SurfaceEvent::Usage(usage) => {
            if state.snapshot.usage.revision.get().checked_add(1) != Some(usage.revision.get()) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "usage revision is not contiguous",
                ));
            }
            state.snapshot.usage = usage.clone();
            Ok(())
        }
        SurfaceEvent::Context(context) => {
            let mut context = context.clone();
            if context.window_id.is_legacy_unspecified() {
                context.window_id = match (&state.snapshot.context.compaction, &context.compaction)
                {
                    (
                        CompactionState::Running {
                            operation_id: before,
                            ..
                        },
                        CompactionState::Completed {
                            operation_id: after,
                            ..
                        },
                    ) if before == after => {
                        ContextWindowId::for_compaction(&state.snapshot.context.window_id, after)
                    }
                    _ => state.snapshot.context.window_id.clone(),
                };
            }
            if state.snapshot.context.revision.get().checked_add(1) != Some(context.revision.get())
                || context.used_tokens > context.limit_tokens
                || !context_window_transition_valid(&state.snapshot.context, &context)
                || matches!(
                    context.compaction,
                    CompactionState::Completed {
                        before_messages,
                        after_messages,
                        collapsed_messages,
                        ..
                    } if after_messages > before_messages
                        || collapsed_messages != before_messages - after_messages
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "context revision or counters are invalid",
                ));
            }
            state.snapshot.context = context;
            Ok(())
        }
        SurfaceEvent::Task(patch) => apply_task_patch(&mut state.snapshot, envelope, patch),
        SurfaceEvent::Interaction(patch) => {
            apply_interaction_patch(&mut state.snapshot, envelope, patch)
        }
        SurfaceEvent::Workflow(patch) => apply_workflow_patch(&mut state.snapshot, envelope, patch),
        SurfaceEvent::Subagent(patch) => apply_subagent_patch(&mut state.snapshot, envelope, patch),
        SurfaceEvent::Goal(goal) => {
            apply_goal_patch(state, envelope, goal)?;
            state.applied_goal_receipts.insert((
                goal.receipt.store_commit_id.clone(),
                goal.receipt.receipt_digest.clone(),
            ));
            Ok(())
        }
        SurfaceEvent::Settings(patch) => apply_settings_patch(&mut state.snapshot, envelope, patch),
        SurfaceEvent::McpCatalog(patch) => {
            apply_mcp_catalog_patch(&mut state.snapshot, envelope, patch)
        }
        SurfaceEvent::PinnedContext(patch) => {
            apply_pinned_context_patch(&mut state.snapshot, envelope, patch)
        }
        SurfaceEvent::Session(patch) => apply_session_patch(state, envelope, patch),
    }
}

fn context_window_transition_valid(
    current: &SurfaceContextSnapshot,
    next: &SurfaceContextSnapshot,
) -> bool {
    let advances_window = matches!(
        (&current.compaction, &next.compaction),
        (
            CompactionState::Running {
                operation_id: before,
                ..
            },
            CompactionState::Completed {
                operation_id: after,
                ..
            }
        ) if before == after
    );
    if advances_window {
        current.window_id != next.window_id
    } else {
        current.window_id == next.window_id
    }
}

fn batch_event_count(
    batch: &SurfaceCommitBatch,
    predicate: impl Fn(&SurfaceEvent) -> bool,
) -> usize {
    batch
        .events
        .as_slice()
        .iter()
        .filter(|envelope| predicate(&envelope.event))
        .count()
}

fn validate_batch_pairings(
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
) -> Result<(), SurfaceReducerError> {
    for envelope in batch.events.as_slice() {
        match &envelope.event {
            SurfaceEvent::Interaction(InteractionPatch::ContinuationDispatchStarted {
                interaction_id,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
                ..
            }) => {
                let identity = state
                    .snapshot
                    .interactions
                    .iter()
                    .find(|interaction| &interaction.interaction_id == interaction_id)
                    .and_then(|interaction| match &interaction.lifecycle {
                        SurfaceInteractionLifecycle::Resolved { receipt }
                            if &receipt.receipt_id == receipt_id =>
                        {
                            DurableInteractionContinuationOperationIdentity::try_new(
                                interaction_id,
                                receipt,
                            )
                            .ok()
                        }
                        _ => None,
                    });
                let Some(identity) = identity else {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "continuation dispatch start lacks its durable resolution identity",
                    ));
                };
                if identity.dispatch_id() != dispatch_id
                    || identity.operation_id() != operation_id
                    || identity.turn_id() != turn_id
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "continuation dispatch start changed its stable operation identity",
                    ));
                }
                let paired_operation = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Operation(OperationPatch::Requested { operation })
                            if &operation.operation_id == identity.operation_id()
                                && &operation.request_id == identity.request_id()
                                && operation.intent.kind == OperationKind::UserTurn
                                && operation.ready_for_admission
                    )
                });
                if paired_operation != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "continuation dispatch start must atomically create its stable operation",
                    ));
                }
            }
            SurfaceEvent::Operation(OperationPatch::Admitted { input, .. }) => match input {
                AdmittedInput::PendingUser {
                    item_id,
                    presentation,
                    correlation_id,
                } => {
                    let count = batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Item(ItemPatch::Added {
                                item: SurfaceItem::UserMessage {
                                    id,
                                    turn_id,
                                    input: SurfaceUserInputState::Pending {
                                        presentation: item_presentation,
                                        correlation_id: item_correlation_id,
                                    },
                                    origin: SurfaceItemOrigin::UserInput,
                                    ..
                                }
                            }) if id == item_id
                                && item_presentation == presentation
                                && item_correlation_id == correlation_id
                        )
                    });
                    if count != 1 {
                        return Err(event_error(
                            envelope,
                            SurfaceReducerErrorCode::InvalidOrdering,
                            "pending admission must pair with exactly one user input item",
                        ));
                    }
                }
                AdmittedInput::NotApplicable => {
                    if batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Item(ItemPatch::Added {
                                item: SurfaceItem::UserMessage { .. }
                            })
                        )
                    }) != 0
                    {
                        return Err(event_error(
                            envelope,
                            SurfaceReducerErrorCode::InvalidOrdering,
                            "non-input admission cannot pair with a user input item",
                        ));
                    }
                }
            },
            SurfaceEvent::Operation(OperationPatch::GenerationReserved { generation }) => {
                if let GenerationInputState::Pending {
                    input_item_id,
                    presentation,
                    correlation_id,
                } = &generation.input
                {
                    let count = batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Item(ItemPatch::Added {
                                item: SurfaceItem::UserMessage {
                                    id,
                                    turn_id,
                                    input: SurfaceUserInputState::Pending {
                                        presentation: item_presentation,
                                        correlation_id: item_correlation_id,
                                    },
                                    origin: SurfaceItemOrigin::GoalContinuation,
                                    ..
                                }
                            }) if id == input_item_id
                                && item_presentation == presentation
                                && item_correlation_id == correlation_id
                                && turn_id == &generation.logical_turn_id
                        )
                    });
                    let invalid_goal_pairing = generation.goal_identity.is_some()
                        && match generation.attempt {
                            GenerationAttempt::Initial => count != 1,
                            GenerationAttempt::RecoveryReplacement => count != 0,
                        };
                    if invalid_goal_pairing {
                        return Err(event_error(
                            envelope,
                            SurfaceReducerErrorCode::InvalidOrdering,
                            "goal generation reservation has an invalid pending item pairing",
                        ));
                    }
                }
            }
            SurfaceEvent::Operation(OperationPatch::InputBindingsResolved {
                input_item_id,
                fact,
                ..
            }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Item(ItemPatch::InputResolved {
                            item_id,
                            fact: item_fact,
                        }) if item_id == input_item_id && item_fact == fact
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "input resolution operation fact lacks exactly one item pair",
                    ));
                }
            }
            SurfaceEvent::Operation(OperationPatch::InputBindingsFailed {
                input_item_id,
                code,
                message,
                ..
            }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Item(ItemPatch::InputResolutionFailed {
                            item_id,
                            code: item_code,
                            message: item_message,
                        }) if item_id == input_item_id
                            && item_code == code
                            && item_message == message
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "input failure operation fact lacks exactly one item pair",
                    ));
                }
            }
            SurfaceEvent::Item(ItemPatch::InputResolved { item_id, fact }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Operation(OperationPatch::InputBindingsResolved {
                            input_item_id,
                            fact: operation_fact,
                            ..
                        }) if input_item_id == item_id && operation_fact == fact
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "input item resolution lacks exactly one operation pair",
                    ));
                }
            }
            SurfaceEvent::Item(ItemPatch::InputResolutionFailed {
                item_id,
                code,
                message,
            }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Operation(OperationPatch::InputBindingsFailed {
                            input_item_id,
                            code: operation_code,
                            message: operation_message,
                            ..
                        }) if input_item_id == item_id
                            && operation_code == code
                            && operation_message == message
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "input failure item lacks exactly one operation pair",
                    ));
                }
            }
            SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Item(ItemPatch::Added {
                            item: SurfaceItem::ToolResultMessage {
                                turn_id,
                                tool_call_id,
                                content,
                                terminal,
                                ..
                            }
                        }) if result_turn_id(state, &result.tool_call_id) == Some(turn_id)
                            && tool_call_id == &result.tool_call_id
                            && terminal == &result.terminal
                            && result.output.as_ref().or(result.error.as_ref()) == Some(content)
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "tool completion must pair with exactly one result item",
                    ));
                }
            }
            SurfaceEvent::Item(ItemPatch::Added {
                item:
                    SurfaceItem::ToolResultMessage {
                        turn_id,
                        tool_call_id,
                        content,
                        terminal,
                        ..
                    },
            }) => {
                let count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Tool(ToolPatch::Completed { result })
                            if result.tool_call_id == *tool_call_id
                                && result.terminal == *terminal
                                && result.output.as_ref().or(result.error.as_ref()) == Some(content)
                                && result_turn_id(state, &result.tool_call_id) == Some(turn_id)
                    )
                });
                if count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "tool result item lacks exactly one completion pair",
                    ));
                }
            }
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call }) => {
                let matching_lease_count = match &call.state {
                    SurfaceCapabilityCallState::Completed {
                        result: CapabilityCallResult::TerminalCreated { terminal_id },
                        ..
                    } => batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                                lease: SurfaceRemoteTerminalLease {
                                    owning_tool_call_id,
                                    state: SurfaceRemoteTerminalLeaseState::Live {
                                        terminal_id: lease_terminal_id,
                                        owner_fence,
                                    },
                                    ..
                                }
                            }) if owning_tool_call_id == &call.owning_tool_call_id
                                && lease_terminal_id == terminal_id
                                && owner_fence == &call.fence
                        )
                    }),
                    SurfaceCapabilityCallState::Completed {
                        result: CapabilityCallResult::TerminalKillAcknowledged,
                        ..
                    } => batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                                lease: SurfaceRemoteTerminalLease {
                                    owning_tool_call_id,
                                    state: SurfaceRemoteTerminalLeaseState::ReleasePending {
                                        owner_fence,
                                        ..
                                    },
                                    ..
                                }
                            }) if owning_tool_call_id == &call.owning_tool_call_id
                                && owner_fence == &call.fence
                        )
                    }),
                    SurfaceCapabilityCallState::Completed {
                        result: CapabilityCallResult::TerminalReleaseAcknowledged,
                        ..
                    } => batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                                lease: SurfaceRemoteTerminalLease {
                                    owning_tool_call_id,
                                    state: SurfaceRemoteTerminalLeaseState::Released,
                                    ..
                                }
                            }) if owning_tool_call_id == &call.owning_tool_call_id
                        )
                    }),
                    SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                        effect_kind: ExternalEffectKind::TerminalCreate,
                        ..
                    } => batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                                lease: SurfaceRemoteTerminalLease {
                                    owning_tool_call_id,
                                    state: SurfaceRemoteTerminalLeaseState::IdentityUnknown {
                                        create_call_id,
                                    },
                                    ..
                                }
                            }) if owning_tool_call_id == &call.owning_tool_call_id
                                && create_call_id == &call.call_id
                        )
                    }),
                    SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                        effect_kind:
                            ExternalEffectKind::TerminalKill | ExternalEffectKind::TerminalRelease,
                        ..
                    } => batch_event_count(batch, |event| {
                        matches!(
                            event,
                            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                                lease: SurfaceRemoteTerminalLease {
                                    owning_tool_call_id,
                                    state: SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                        terminal_id: Some(terminal_id),
                                        owner_fence,
                                    },
                                    ..
                                }
                            }) if owning_tool_call_id == &call.owning_tool_call_id
                                && owner_fence == &call.fence
                                && call.arguments_digest
                                    == Sha256Digest::new(
                                        sha2::Sha256::digest(
                                            terminal_id.as_str().as_bytes()
                                        ).into()
                                    )
                        )
                    }),
                    _ => continue,
                };
                if matching_lease_count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "terminal capability settlement lacks exactly one lease fact",
                    ));
                }
            }
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                lease:
                    SurfaceRemoteTerminalLease {
                        owning_tool_call_id,
                        state: SurfaceRemoteTerminalLeaseState::IdentityUnknown { create_call_id },
                        ..
                    },
            }) => {
                let matching_create_count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged {
                            call: SurfaceCapabilityCall {
                                call_id,
                                owning_tool_call_id: call_tool_id,
                                kind: SurfaceCapabilityCallKind::TerminalCreate,
                                state: SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind: ExternalEffectKind::TerminalCreate,
                                    ..
                                },
                                ..
                            }
                        }) if call_id == create_call_id && call_tool_id == owning_tool_call_id
                    )
                });
                if matching_create_count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "identity-unknown lease lacks its terminal-create ambiguity",
                    ));
                }
            }
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                lease:
                    SurfaceRemoteTerminalLease {
                        owning_tool_call_id,
                        state:
                            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                terminal_id: Some(terminal_id),
                                owner_fence,
                            },
                        ..
                    },
            }) => {
                let matching_cleanup_count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged {
                            call: SurfaceCapabilityCall {
                                owning_tool_call_id: call_tool_id,
                                fence: call_fence,
                                arguments_digest,
                                kind,
                                state: SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind,
                                    ..
                                },
                                ..
                            }
                        }) if call_tool_id == owning_tool_call_id
                            && call_fence == owner_fence
                            && arguments_digest
                                == &Sha256Digest::new(
                                    sha2::Sha256::digest(
                                        terminal_id.as_str().as_bytes()
                                    ).into()
                                )
                            && matches!(
                                (kind, effect_kind),
                                (
                                    SurfaceCapabilityCallKind::TerminalKill,
                                    ExternalEffectKind::TerminalKill
                                ) | (
                                    SurfaceCapabilityCallKind::TerminalRelease,
                                    ExternalEffectKind::TerminalRelease
                                )
                            )
                    )
                });
                if matching_cleanup_count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "cleanup-ambiguous lease lacks its terminal cleanup ambiguity",
                    ));
                }
            }
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                lease:
                    SurfaceRemoteTerminalLease {
                        owning_tool_call_id,
                        state: SurfaceRemoteTerminalLeaseState::Released,
                        ..
                    },
            }) => {
                let release_ack_count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged {
                            call: SurfaceCapabilityCall {
                                owning_tool_call_id: call_tool_id,
                                kind: SurfaceCapabilityCallKind::TerminalRelease,
                                state: SurfaceCapabilityCallState::Completed {
                                    result: CapabilityCallResult::TerminalReleaseAcknowledged,
                                    ..
                                },
                                ..
                            }
                        }) if call_tool_id == owning_tool_call_id
                    )
                });
                if release_ack_count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "released terminal lease lacks exactly one release acknowledgement",
                    ));
                }
            }
            SurfaceEvent::Goal(GoalPatchEnvelope {
                patch:
                    GoalPatch::ContinuationDecided {
                        predecessor,
                        decision,
                        ..
                    },
                ..
            }) => {
                let stopped_count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                            fence,
                            ..
                        }) if fence == &predecessor.operation_fence
                    )
                });
                let settled_count = batch_event_count(batch, |event| {
                    matches!(
                        event,
                        SurfaceEvent::Goal(GoalPatchEnvelope {
                            patch: GoalPatch::OuterTurnFinished { identity, .. },
                            ..
                        }) if identity == predecessor
                    )
                });
                if stopped_count != 1 || settled_count != 1 {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "goal continuation lacks its exact stop and outer-turn settlement",
                    ));
                }
                match decision {
                    GoalContinuationDecision::Admitted { successor, .. } => {
                        let successful_stop_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                                    fence,
                                    reason: GenerationStopReason::Completed {
                                        status:
                                            GenerationCompletionStatus::Success
                                            | GenerationCompletionStatus::BudgetExhausted {
                                                budget:
                                                    OperationBudget::TurnRequests {
                                                        scope:
                                                            TurnRequestBudgetScope::AgentLoop,
                                                        ..
                                                    },
                                            },
                                    },
                                    ..
                                }) if fence == &predecessor.operation_fence
                            )
                        });
                        if successful_stop_count != 1 {
                            return Err(event_error(
                                envelope,
                                SurfaceReducerErrorCode::InvalidOrdering,
                                "admitted goal continuation requires a resumable predecessor stop",
                            ));
                        }
                        let reserved_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                                    generation
                                }) if generation.fence == successor.operation_fence
                                    && generation.goal_identity.as_ref() == Some(successor)
                            )
                        });
                        let finalization_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                                    operation_id,
                                    ..
                                }) if operation_id == &predecessor.operation_fence.operation_id
                            )
                        });
                        if reserved_count != 1 || finalization_count != 0 {
                            return Err(event_error(
                                envelope,
                                SurfaceReducerErrorCode::InvalidOrdering,
                                "admitted goal continuation lacks its unique successor reservation",
                            ));
                        }
                    }
                    GoalContinuationDecision::Stopped {
                        reason,
                        goal_state,
                        terminal,
                        ..
                    } => {
                        let finalization_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                                    operation_id,
                                    ..
                                }) if operation_id == &predecessor.operation_fence.operation_id
                            )
                        });
                        let matching_finalization_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                                    operation_id,
                                    selected_cause,
                                    ..
                                }) if operation_id == &predecessor.operation_fence.operation_id
                                    && goal_stop_terminal_matches(selected_cause, terminal)
                                    && goal_stop_terminal_binding_matches(
                                        reason,
                                        goal_state,
                                        terminal,
                                    )
                            )
                        });
                        let reserved_count = batch_event_count(batch, |event| {
                            matches!(
                                event,
                                SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                                    generation
                                }) if generation.predecessor.as_ref() == Some(&predecessor.operation_fence)
                            )
                        });
                        if finalization_count != 1 || reserved_count != 0 {
                            return Err(event_error(
                                envelope,
                                SurfaceReducerErrorCode::InvalidOrdering,
                                "stopped goal continuation lacks its unique finalization",
                            ));
                        }
                        if matching_finalization_count != 1 {
                            return Err(event_error(
                                envelope,
                                SurfaceReducerErrorCode::GoalReceiptMismatch,
                                "stopped goal continuation terminal mapping is invalid",
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Pair every identity-bearing Goal generation fact with the complete operation view that
    // survived this batch. Goal facts cannot temporarily exist without their GenerationRecord.
    for envelope in batch.events.as_slice() {
        let identity = match &envelope.event {
            SurfaceEvent::Goal(GoalPatchEnvelope {
                patch:
                    GoalPatch::OuterTurnStarted { identity }
                    | GoalPatch::OuterTurnFinished { identity, .. }
                    | GoalPatch::VerificationCompleted { identity, .. },
                ..
            }) => Some(identity),
            _ => None,
        };
        if let Some(identity) = identity {
            let generation =
                snapshot_generation_record(state.snapshot(), &identity.operation_fence);
            if generation
                .is_none_or(|generation| generation.goal_identity.as_ref() != Some(identity))
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal generation fact lacks its exact operation generation",
                ));
            }
        }
    }
    Ok(())
}

fn result_turn_id<'a>(
    state: &'a SurfaceReducerState,
    tool_call_id: &SurfaceToolCallId,
) -> Option<&'a SurfaceTurnId> {
    state
        .snapshot()
        .tools
        .iter()
        .find(|tool| &tool.request.tool_call_id == tool_call_id)
        .map(|tool| &tool.request.turn_id)
}

fn goal_stop_terminal_matches(
    cause: &OperationFinalizationCause,
    terminal: &OperationTerminal,
) -> bool {
    match (cause, terminal) {
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                status: GenerationCompletionStatus::Success,
            }),
            OperationTerminal::Succeeded { .. },
        ) => true,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                status: GenerationCompletionStatus::BudgetExhausted { budget },
            }),
            OperationTerminal::BudgetExhausted {
                budget: terminal_budget,
            },
        ) => budget == terminal_budget,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                status: GenerationCompletionStatus::VerificationFailed { message },
            }),
            OperationTerminal::Failed {
                class: FailureClass::Verification,
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Cancelled {
                cause: TerminalizationCause::UserCancel,
            }),
            OperationTerminal::Cancelled {
                reason: CancelReason::User,
            },
        ) => true,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Cancelled {
                cause: TerminalizationCause::GoalPause,
            }),
            OperationTerminal::Cancelled {
                reason: CancelReason::GoalPause,
            },
        ) => true,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Cancelled {
                cause: TerminalizationCause::HostShutdown,
            }),
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            },
        ) => true,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::Cancelled {
                cause: TerminalizationCause::ThreadClose,
            }),
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::ThreadClose,
            },
        ) => true,
        (
            OperationFinalizationCause::GenerationStop(GenerationStopReason::ExecutionFailed {
                class,
                message,
            }),
            OperationTerminal::Failed {
                class: terminal_class,
                message: terminal_message,
            },
        ) => {
            generation_failure_maps_to_terminal(*class, terminal_class)
                && message == terminal_message
        }
        _ => false,
    }
}

fn goal_stop_terminal_binding_matches(
    reason: &GoalContinuationStopReason,
    goal_state: &SurfaceGoalState,
    terminal: &OperationTerminal,
) -> bool {
    let succeeded = matches!(terminal, OperationTerminal::Succeeded { .. });
    match reason {
        GoalContinuationStopReason::GoalInactive { state } => match state {
            SurfaceGoalState::Paused {
                reason: SurfaceGoalPauseReason::UsageLimit,
                ..
            } => {
                state == goal_state
                    && matches!(
                        terminal,
                        OperationTerminal::BudgetExhausted {
                            budget: OperationBudget::MonetaryBudgetUsdMicros { .. },
                        }
                    )
            }
            SurfaceGoalState::Paused { .. }
            | SurfaceGoalState::Blocked { .. }
            | SurfaceGoalState::Complete { .. } => state == goal_state && succeeded,
            SurfaceGoalState::BudgetLimited => {
                matches!(goal_state, SurfaceGoalState::BudgetLimited)
                    && matches!(terminal, OperationTerminal::BudgetExhausted { .. })
            }
            SurfaceGoalState::Active => false,
        },
        GoalContinuationStopReason::BudgetLimited { budget } => {
            matches!(goal_state, SurfaceGoalState::BudgetLimited)
                && matches!(
                    terminal,
                    OperationTerminal::BudgetExhausted { budget: terminal_budget }
                        if terminal_budget == budget.as_budget()
                )
        }
        GoalContinuationStopReason::PredecessorNotSuccessful {
            terminal: predecessor_terminal,
            ..
        } => terminal == predecessor_terminal && !matches!(goal_state, SurfaceGoalState::Active),
        GoalContinuationStopReason::TerminalizingControl { cause } => match cause {
            TerminalizationCause::UserCancel => {
                matches!(
                    (goal_state, terminal),
                    (
                        SurfaceGoalState::Paused {
                            reason: SurfaceGoalPauseReason::User,
                            ..
                        },
                        OperationTerminal::Cancelled {
                            reason: CancelReason::User,
                        }
                    )
                )
            }
            TerminalizationCause::GoalPause => {
                matches!(
                    (goal_state, terminal),
                    (
                        SurfaceGoalState::Paused {
                            reason: SurfaceGoalPauseReason::User,
                            ..
                        },
                        OperationTerminal::Cancelled {
                            reason: CancelReason::GoalPause,
                        }
                    )
                )
            }
            TerminalizationCause::HostShutdown => {
                matches!(
                    (goal_state, terminal),
                    (
                        SurfaceGoalState::Paused {
                            reason: SurfaceGoalPauseReason::Infrastructure,
                            ..
                        },
                        OperationTerminal::Shutdown {
                            reason: SurfaceShutdownReason::HostShutdown,
                        }
                    )
                )
            }
            TerminalizationCause::ThreadClose => {
                matches!(
                    (goal_state, terminal),
                    (
                        SurfaceGoalState::Paused {
                            reason: SurfaceGoalPauseReason::Infrastructure,
                            ..
                        },
                        OperationTerminal::Shutdown {
                            reason: SurfaceShutdownReason::ThreadClose,
                        }
                    )
                )
            }
        },
        GoalContinuationStopReason::QueuedUserInput { .. }
        | GoalContinuationStopReason::PendingInteraction { .. } => {
            let expected_pause = match reason {
                GoalContinuationStopReason::QueuedUserInput { .. } => SurfaceGoalPauseReason::User,
                GoalContinuationStopReason::PendingInteraction { .. } => {
                    SurfaceGoalPauseReason::Infrastructure
                }
                _ => unreachable!(),
            };
            matches!(
                goal_state,
                SurfaceGoalState::Paused { reason, .. } if *reason == expected_pause
            ) && succeeded
        }
        GoalContinuationStopReason::WorkflowOwned { .. } => {
            matches!(
                goal_state,
                SurfaceGoalState::Paused {
                    reason: SurfaceGoalPauseReason::WaitingForWorkflow,
                    ..
                }
            ) && succeeded
        }
        GoalContinuationStopReason::PlanModeDisallowsContinuation
        | GoalContinuationStopReason::VerificationPending => {
            matches!(
                goal_state,
                SurfaceGoalState::Paused {
                    reason: SurfaceGoalPauseReason::NoProgress,
                    ..
                }
            ) && succeeded
        }
        GoalContinuationStopReason::RuntimeFailure { class, message } => {
            matches!(
                (goal_state, terminal),
                (
                    SurfaceGoalState::Paused {
                        reason: SurfaceGoalPauseReason::Infrastructure,
                        ..
                    },
                    OperationTerminal::Failed {
                        class: terminal_class,
                        message: terminal_message,
                    }
                ) if terminal_class == class && terminal_message == message
            )
        }
    }
}

fn generation_failure_maps_to_terminal(
    class: GenerationExecutionFailureClass,
    terminal_class: &FailureClass,
) -> bool {
    matches!(
        (class, terminal_class),
        (
            GenerationExecutionFailureClass::Provider,
            FailureClass::Provider
        ) | (GenerationExecutionFailureClass::Tool, FailureClass::Tool)
            | (GenerationExecutionFailureClass::Hook, FailureClass::Hook)
            | (
                GenerationExecutionFailureClass::Workflow,
                FailureClass::Workflow
            )
            | (
                GenerationExecutionFailureClass::InputResolution,
                FailureClass::InputResolution
            )
            | (
                GenerationExecutionFailureClass::ClientCapabilityUnavailable,
                FailureClass::ClientCapabilityUnavailable
            )
            | (
                GenerationExecutionFailureClass::LegacyApprovalRequired,
                FailureClass::LegacyApprovalRequired
            )
            | (
                GenerationExecutionFailureClass::RuntimeInvariant,
                FailureClass::RuntimeInvariant
            )
            | (
                GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                FailureClass::ExternalEffectAmbiguous
            )
            | (
                GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous,
                FailureClass::RemoteResourceCleanupAmbiguous
            )
    )
}

fn terminalization_cause_matches_terminal(
    cause: TerminalizationCause,
    terminal: &OperationTerminal,
) -> bool {
    matches!(
        (cause, terminal),
        (
            TerminalizationCause::UserCancel,
            OperationTerminal::Cancelled {
                reason: CancelReason::User,
            }
        ) | (
            TerminalizationCause::GoalPause,
            OperationTerminal::Cancelled {
                reason: CancelReason::GoalPause,
            }
        ) | (
            TerminalizationCause::HostShutdown,
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            }
        ) | (
            TerminalizationCause::ThreadClose,
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::ThreadClose,
            }
        )
    )
}

fn operation_replayability_is_not_current(
    snapshot: &SurfaceSnapshot,
    operation: &OperationRecord,
) -> bool {
    let replayability = operation
        .generations
        .last()
        .map(|generation| &generation.replayability)
        .unwrap_or(&operation.intent.initial_replayability);
    matches!(
        replayability,
        Replayability::NonReplayable {
            live_capsule: LiveOperationCapsule::Unavailable,
            ..
        }
    ) || matches!(
        replayability,
        Replayability::NonReplayable {
            live_capsule: LiveOperationCapsule::Available { incarnation },
            ..
        } if incarnation != &snapshot.cursor.incarnation
    )
}

fn terminal_names_last_generation(
    operation: &OperationRecord,
    terminal_generation: SurfaceGenerationId,
) -> bool {
    operation
        .generations
        .last()
        .is_some_and(|generation| generation.fence.generation_id == terminal_generation)
}

fn reservation_finalizer_matches_terminal(
    reason: &ReservationFinalizerReason,
    terminal: &OperationTerminal,
) -> bool {
    matches!(
        (reason, terminal),
        (
            ReservationFinalizerReason::ReservationExpired,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ReservationExpired,
            }
        ) | (
            ReservationFinalizerReason::AdmissionRejected {
                reason: AdmissionRejectionReason::ConfigurationConflict,
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ConfigurationConflict,
            }
        ) | (
            ReservationFinalizerReason::AdmissionRejected {
                reason: AdmissionRejectionReason::PolicyConflict,
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::PolicyConflict,
            }
        ) | (
            ReservationFinalizerReason::CancelledBeforeAdmission,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::CancelledBeforeAdmission,
            }
        ) | (
            ReservationFinalizerReason::RuntimeRestart,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::RuntimeRestart,
            }
        ) | (
            ReservationFinalizerReason::HostShutdown,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::HostShutdown,
            }
        ) | (
            ReservationFinalizerReason::ThreadClose,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ThreadClose,
            }
        )
    )
}

fn generation_stop_matches_terminal(
    snapshot: &SurfaceSnapshot,
    operation: &OperationRecord,
    reason: &GenerationStopReason,
    terminal: &OperationTerminal,
) -> bool {
    match (reason, terminal) {
        (
            GenerationStopReason::Completed {
                status: GenerationCompletionStatus::Success,
            },
            OperationTerminal::Succeeded { .. },
        ) => true,
        (
            GenerationStopReason::Completed {
                status: GenerationCompletionStatus::VerificationFailed { message },
            },
            OperationTerminal::Failed {
                class: FailureClass::Verification,
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            GenerationStopReason::Completed {
                status: GenerationCompletionStatus::BudgetExhausted { budget },
            },
            OperationTerminal::BudgetExhausted {
                budget: terminal_budget,
            },
        ) => budget == terminal_budget,
        (GenerationStopReason::Cancelled { cause }, terminal) => {
            terminalization_cause_matches_terminal(*cause, terminal)
        }
        (
            GenerationStopReason::InterruptedResumable | GenerationStopReason::ProviderSuspended,
            OperationTerminal::AbortedByRuntimeRestart { last_generation },
        ) => {
            operation_replayability_is_not_current(snapshot, operation)
                && terminal_names_last_generation(operation, *last_generation)
        }
        (
            GenerationStopReason::RuntimeRestart,
            OperationTerminal::AbortedByRuntimeRestart { last_generation },
        ) => terminal_names_last_generation(operation, *last_generation),
        (
            GenerationStopReason::ProjectionFailure { message },
            OperationTerminal::Failed {
                class: FailureClass::Persistence,
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            GenerationStopReason::ExecutionFailed { class, message },
            OperationTerminal::Failed {
                class: terminal_class,
                message: terminal_message,
            },
        ) => {
            generation_failure_maps_to_terminal(*class, terminal_class)
                && message == terminal_message
        }
        (
            GenerationStopReason::Panicked { message },
            OperationTerminal::Panicked {
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled { cause },
            },
            terminal,
        ) => terminalization_cause_matches_terminal(*cause, terminal),
        (
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Interrupted | NotStartedReason::RuntimeRestart,
            },
            OperationTerminal::AbortedByRuntimeRestart { last_generation },
        ) => {
            operation_replayability_is_not_current(snapshot, operation)
                && terminal_names_last_generation(operation, *last_generation)
        }
        (
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::ReservationExpired,
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ReservationExpired,
            },
        ) => operation
            .generations
            .iter()
            .all(|generation| generation.started_witness.is_none()),
        (
            GenerationStopReason::NotStarted {
                reason:
                    NotStartedReason::AdmissionRejected {
                        reason: AdmissionRejectionReason::ConfigurationConflict,
                    },
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ConfigurationConflict,
            },
        )
        | (
            GenerationStopReason::NotStarted {
                reason:
                    NotStartedReason::AdmissionRejected {
                        reason: AdmissionRejectionReason::PolicyConflict,
                    },
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::PolicyConflict,
            },
        ) => operation
            .generations
            .iter()
            .all(|generation| generation.started_witness.is_none()),
        (
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::StartCommitFailure { message },
            },
            OperationTerminal::Failed {
                class: FailureClass::Persistence,
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::MissingLiveInputCapsule,
            },
            OperationTerminal::Failed {
                class: FailureClass::RuntimeInvariant,
                message,
            },
        ) => {
            message.as_str()
                == "non-replayable operation input capsule is unavailable before generation start"
        }
        (
            GenerationStopReason::NotStarted {
                reason:
                    NotStartedReason::Shutdown {
                        reason: SurfaceShutdownReason::HostShutdown,
                    },
            },
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            },
        )
        | (
            GenerationStopReason::NotStarted {
                reason:
                    NotStartedReason::Shutdown {
                        reason: SurfaceShutdownReason::ThreadClose,
                    },
            },
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::ThreadClose,
            },
        ) => true,
        _ => false,
    }
}

fn suspended_finalization_matches_terminal(
    cause: &SuspendedFinalizationCause,
    terminal: &OperationTerminal,
) -> bool {
    match (cause, terminal) {
        (SuspendedFinalizationCause::Terminalization(cause), terminal) => {
            terminalization_cause_matches_terminal(*cause, terminal)
        }
        (
            SuspendedFinalizationCause::ResumeStartCommitFailure { message },
            OperationTerminal::Failed {
                class: FailureClass::Persistence,
                message: terminal_message,
            },
        ) => message == terminal_message,
        (
            SuspendedFinalizationCause::RecoveryAbortNonReplayable { last_generation },
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: terminal_generation,
            },
        ) => last_generation == terminal_generation,
        _ => false,
    }
}

fn operation_finalization_matches_terminal(
    snapshot: &SurfaceSnapshot,
    operation: &OperationRecord,
    finalization: &OperationFinalizationRecord,
    record: &OperationTerminalRecord,
) -> bool {
    match (&finalization.selected_cause, &record.terminal) {
        (OperationFinalizationCause::Terminalization(cause), terminal) => {
            terminalization_cause_matches_terminal(*cause, terminal)
        }
        (OperationFinalizationCause::GenerationStop(reason), terminal) => {
            generation_stop_matches_terminal(snapshot, operation, reason, terminal)
        }
        (OperationFinalizationCause::Reservation(reason), terminal) => {
            reservation_finalizer_matches_terminal(reason, terminal)
        }
        (
            OperationFinalizationCause::OperationJoinSettlement(source),
            OperationTerminal::JoinFailed { message },
        ) => {
            source.operation_id == record.operation_id
                && source.finalize_intent_id == record.finalize_intent_id
                && source.message == *message
                && record.settlement_receipts.iter().any(|receipt| {
                    receipt.settlement_id == source.settlement_id
                        && receipt.receipt_digest == source.settlement_receipt_digest
                })
        }
        (OperationFinalizationCause::OperationJoinSettlement(_), _) => false,
        (OperationFinalizationCause::Suspended(cause), terminal) => {
            finalization.suspended_cause.as_ref() == Some(cause)
                && suspended_finalization_matches_terminal(cause, terminal)
        }
    }
}

fn health_issue_matches_id(id: &SurfaceHealthIssueId, issue: &SurfaceHealthIssue) -> bool {
    matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::Mutation(expected),
            SurfaceHealthIssue::MutationDegraded { settlement_id }
        ) if expected == settlement_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::Projection(expected),
            SurfaceHealthIssue::ProjectionDegraded { commit_id, .. }
        ) if expected == commit_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::StartCommit(expected),
            SurfaceHealthIssue::StartCommitDegraded { commit_id, .. }
        ) if expected == commit_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::Finalization(expected),
            SurfaceHealthIssue::FinalizingDegraded { finalize_intent_id, .. }
        ) if expected == finalize_intent_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::BackgroundFinalization(expected),
            SurfaceHealthIssue::BackgroundFinalizingDegraded { finalize_intent_id, .. }
        ) if expected == finalize_intent_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::Capability(expected),
            SurfaceHealthIssue::CapabilityObservationUnavailable { call_id }
                | SurfaceHealthIssue::ExternalEffectAmbiguous { call_id }
        ) if expected == call_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::RemoteTerminal(expected),
            SurfaceHealthIssue::RemoteTerminalIdentityUnknown { lease_id }
                | SurfaceHealthIssue::RemoteTerminalCleanupAmbiguous { lease_id }
        ) if expected == lease_id
    ) || matches!(
        (id, issue),
        (
            SurfaceHealthIssueId::Ownership(expected),
            SurfaceHealthIssue::OwnershipLost { stale_epoch }
        ) if expected == stale_epoch
    )
}

fn apply_session_patch(
    state: &mut SurfaceReducerState,
    envelope: &SurfaceEventEnvelope,
    patch: &SessionPatch,
) -> Result<(), SurfaceReducerError> {
    let snapshot = &mut state.snapshot;
    match patch {
        SessionPatch::Materialized { thread } => {
            if thread.thread_id != snapshot.thread.thread_id
                || thread.owner_epoch < snapshot.thread.owner_epoch
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "materialized thread identity or owner epoch is stale",
                ));
            }
            snapshot.thread = thread.clone();
            Ok(())
        }
        SessionPatch::OwnerEpochChanged { previous, next } => {
            let recorded_owner_matches = matches!(
                &envelope.commit_class,
                CommitClass::Recorded {
                    thread_owner_epoch,
                    ..
                } if thread_owner_epoch == next
            );
            if snapshot.thread.owner_epoch != *previous
                || previous >= next
                || !recorded_owner_matches
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "thread owner epoch transition is not authorized",
                ));
            }
            snapshot.thread.owner_epoch = *next;
            Ok(())
        }
        SessionPatch::MetadataChanged {
            previous_revision,
            next_revision,
            title,
            updated_at,
        } => {
            if snapshot.thread.metadata_revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
                || *updated_at < snapshot.thread.updated_at
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "session metadata revision or timestamp is stale",
                ));
            }
            snapshot.thread.metadata_revision = *next_revision;
            snapshot.thread.title = title.clone();
            snapshot.thread.updated_at = *updated_at;
            Ok(())
        }
        SessionPatch::HealthIssueAdded {
            previous_revision,
            next_revision,
            id,
            issue,
        } => {
            if snapshot.session_health.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "session health revision is stale",
                ));
            }
            if !health_issue_matches_id(id, issue)
                || snapshot
                    .session_health
                    .issues
                    .iter()
                    .any(|(current, _)| current == id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "session health issue identity is invalid or duplicated",
                ));
            }
            snapshot
                .session_health
                .issues
                .push((id.clone(), issue.clone()));
            snapshot.session_health.revision = *next_revision;
            Ok(())
        }
        SessionPatch::HealthIssueCleared {
            previous_revision,
            next_revision,
            id,
            proof,
        } => {
            if snapshot.session_health.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "session health revision is stale",
                ));
            }
            if proof.issue_id != *id {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "health clear proof names a different issue",
                ));
            }
            let position = snapshot
                .session_health
                .issues
                .iter()
                .position(|(current, _)| current == id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "session health issue does not exist",
                    )
                })?;
            snapshot.session_health.issues.remove(position);
            snapshot.session_health.revision = *next_revision;
            Ok(())
        }
        SessionPatch::RuntimeFault { .. } => Ok(()),
        SessionPatch::Closing {
            reason,
            barrier_id,
            closing_commit_id,
            plan_digest,
        } => {
            if snapshot.session_health.closing || snapshot.session_health.closed {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "session close barrier is already active",
                ));
            }
            snapshot.session_health.accepting_admission = false;
            snapshot.session_health.closing = true;
            state.session_close = Some(SessionCloseWitness {
                reason: *reason,
                barrier_id: barrier_id.clone(),
                closing_commit_id: closing_commit_id.clone(),
                plan_digest: plan_digest.clone(),
            });
            Ok(())
        }
        SessionPatch::Closed {
            reason,
            barrier_id,
            closing_commit_id,
            plan_digest,
        } => {
            let open_interaction = snapshot.interactions.iter().any(|interaction| {
                matches!(
                    interaction.lifecycle,
                    SurfaceInteractionLifecycle::Requested
                        | SurfaceInteractionLifecycle::Transferred { .. }
                )
            });
            if !snapshot.session_health.closing
                || snapshot.session_health.closed
                || snapshot.foreground_operation.is_some()
                || !snapshot.queued_operations.is_empty()
                || !snapshot.background_operations.is_empty()
                || open_interaction
                || state.session_close.as_ref()
                    != Some(&SessionCloseWitness {
                        reason: *reason,
                        barrier_id: barrier_id.clone(),
                        closing_commit_id: closing_commit_id.clone(),
                        plan_digest: plan_digest.clone(),
                    })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "session cannot close before owned work settles",
                ));
            }
            snapshot.thread.closed = true;
            snapshot.session_health.closed = true;
            Ok(())
        }
    }
}

fn apply_settings_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &SettingsPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        SettingsPatch::Committed {
            previous_revision,
            snapshot: next,
        } => {
            if snapshot.settings.thread_revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next.thread_revision.get())
                || next.host_revision < snapshot.settings.host_revision
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "settings committed revision is stale",
                ));
            }
            snapshot.settings = next.clone();
            Ok(())
        }
        SettingsPatch::PendingChanged {
            thread_revision,
            pending,
        } => {
            if snapshot.settings.thread_revision.get().checked_add(1) != Some(thread_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "settings pending revision is not contiguous",
                ));
            }
            snapshot.settings.thread_revision = *thread_revision;
            snapshot.settings.pending = pending.clone();
            Ok(())
        }
    }
}

fn apply_mcp_catalog_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &McpCatalogPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        McpCatalogPatch::Reconciled {
            previous_revision,
            snapshot: next,
        } => {
            if snapshot.mcp_catalog.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next.revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "MCP catalog reconciliation revision is stale",
                ));
            }
            let mut ids = HashSet::new();
            if !next
                .tools
                .iter()
                .map(|entry| &entry.id)
                .chain(next.resources.iter().map(|entry| &entry.id))
                .chain(next.resource_templates.iter().map(|entry| &entry.id))
                .all(|id| ids.insert(id.clone()))
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "MCP catalog entry identity repeats",
                ));
            }
            snapshot.mcp_catalog = next.clone();
            Ok(())
        }
        McpCatalogPatch::ServerStatusChanged {
            previous_revision,
            next_revision,
            server,
            status,
        } => {
            if snapshot.mcp_catalog.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "MCP server status revision is stale",
                ));
            }
            if let Some((_, current)) = snapshot
                .mcp_catalog
                .servers
                .iter_mut()
                .find(|(name, _)| name == server)
            {
                *current = status.clone();
            } else {
                snapshot
                    .mcp_catalog
                    .servers
                    .push((server.clone(), status.clone()));
            }
            snapshot.mcp_catalog.revision = *next_revision;
            Ok(())
        }
    }
}

fn pinned_context_source_matches(entry: &SurfacePinnedContextEntry) -> bool {
    matches!(
        (entry.kind, &entry.source_revision),
        (
            SurfacePinnedContextKind::Memory,
            PinnedContextSourceRevision::Memory(_)
        ) | (
            SurfacePinnedContextKind::File,
            PinnedContextSourceRevision::File(_)
        ) | (
            SurfacePinnedContextKind::User,
            PinnedContextSourceRevision::User(_)
        ) | (
            SurfacePinnedContextKind::System,
            PinnedContextSourceRevision::System(_)
        )
    )
}

fn apply_pinned_context_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &PinnedContextPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        PinnedContextPatch::Added {
            previous_revision,
            next_revision,
            entry,
        } => {
            if snapshot.pinned_context.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "pinned context add revision is stale",
                ));
            }
            if !pinned_context_source_matches(entry) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "pinned context kind and source revision differ",
                ));
            }
            if snapshot
                .pinned_context
                .entries
                .iter()
                .any(|current| current.id == entry.id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "pinned context identity already exists",
                ));
            }
            snapshot.pinned_context.entries.push(entry.clone());
            snapshot.pinned_context.revision = *next_revision;
            Ok(())
        }
        PinnedContextPatch::Removed {
            previous_revision,
            next_revision,
            entry_id,
        } => {
            if snapshot.pinned_context.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next_revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "pinned context remove revision is stale",
                ));
            }
            let position = snapshot
                .pinned_context
                .entries
                .iter()
                .position(|entry| entry.id == *entry_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "pinned context entry does not exist",
                    )
                })?;
            snapshot.pinned_context.entries.remove(position);
            snapshot.pinned_context.revision = *next_revision;
            Ok(())
        }
        PinnedContextPatch::Reconciled {
            previous_revision,
            snapshot: next,
        } => {
            if snapshot.pinned_context.revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(next.revision.get())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "pinned context reconciliation revision is stale",
                ));
            }
            let mut ids = HashSet::new();
            if next
                .entries
                .iter()
                .any(|entry| !pinned_context_source_matches(entry) || !ids.insert(entry.id.clone()))
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "pinned context reconciliation is not canonical",
                ));
            }
            snapshot.pinned_context = next.clone();
            Ok(())
        }
    }
}

fn surface_item_id(item: &SurfaceItem) -> &SurfaceItemId {
    match item {
        SurfaceItem::UserMessage { id, .. }
        | SurfaceItem::SystemMessage { id, .. }
        | SurfaceItem::AssistantMessage { id, .. }
        | SurfaceItem::AssistantReasoning { id, .. }
        | SurfaceItem::AssistantPlan { id, .. }
        | SurfaceItem::ToolResultMessage { id, .. } => id,
    }
}

fn snapshot_generation<'a>(
    snapshot: &'a SurfaceSnapshot,
    fence: &SurfaceOperationFence,
) -> Option<&'a GenerationRecord> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(
            snapshot
                .operation_history
                .iter()
                .filter(|operation| operation.terminal.is_none()),
        )
        .flat_map(|operation| operation.generations.iter())
        .find(|generation| generation.fence == *fence)
}

fn snapshot_generation_record<'a>(
    snapshot: &'a SurfaceSnapshot,
    fence: &SurfaceOperationFence,
) -> Option<&'a GenerationRecord> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .flat_map(|operation| operation.generations.iter())
        .find(|generation| generation.fence == *fence)
}

fn generation_fence_for_turn(
    snapshot: &SurfaceSnapshot,
    turn_id: &SurfaceTurnId,
) -> Option<SurfaceOperationFence> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(
            snapshot
                .operation_history
                .iter()
                .filter(|operation| operation.terminal.is_none()),
        )
        .flat_map(|operation| operation.generations.iter())
        .filter(|generation| generation.logical_turn_id == *turn_id)
        .last()
        .map(|generation| generation.fence.clone())
}

fn event_scope_owns_generation(
    snapshot: &SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    fence: &SurfaceOperationFence,
) -> bool {
    if snapshot_generation(snapshot, fence).is_none() {
        return false;
    }
    match &envelope.scope {
        SurfaceScope::Generation { fence: scoped } => {
            scoped == fence
                && !snapshot
                    .background_operations
                    .iter()
                    .any(|operation| operation.operation_id == fence.operation_id)
        }
        SurfaceScope::Background { fence: scoped } => {
            scoped.operation_fence == *fence
                && snapshot
                    .background_operations
                    .iter()
                    .any(|operation| operation.fence == *scoped)
        }
        _ => false,
    }
}

fn require_event_scope_owns_generation(
    snapshot: &SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    fence: &SurfaceOperationFence,
) -> Result<(), SurfaceReducerError> {
    if event_scope_owns_generation(snapshot, envelope, fence) {
        Ok(())
    } else {
        Err(event_error(
            envelope,
            SurfaceReducerErrorCode::ScopeMismatch,
            "event generation owner fence is stale",
        ))
    }
}

fn apply_item_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    batch: &SurfaceCommitBatch,
    patch: &ItemPatch,
) -> Result<(), SurfaceReducerError> {
    let scoped_fence = match &envelope.scope {
        SurfaceScope::Generation { fence } => fence,
        SurfaceScope::Background { fence } => &fence.operation_fence,
        _ => unreachable!("item scope class is validated before reduction"),
    };
    require_event_scope_owns_generation(snapshot, envelope, scoped_fence)?;
    match patch {
        ItemPatch::Added { item } => {
            if snapshot
                .items
                .iter()
                .any(|existing| surface_item_id(existing) == surface_item_id(item))
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "item identity already exists",
                ));
            }
            let paired = match item {
                SurfaceItem::AssistantMessage {
                    id,
                    turn_id,
                    text,
                    pinned,
                } => batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response })
                            if response.message_item.as_ref().is_some_and(|value|
                                value.id == *id
                                    && value.turn_id == *turn_id
                                    && value.text == *text
                                    && value.pinned == *pinned)
                    )
                }),
                SurfaceItem::AssistantReasoning {
                    id,
                    turn_id,
                    summary,
                    content,
                    pinned,
                } => batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response })
                            if response.reasoning_item.as_ref().is_some_and(|value|
                                value.id == *id
                                    && value.turn_id == *turn_id
                                    && value.summary == *summary
                                    && value.content == *content
                                    && value.pinned == *pinned)
                    )
                }),
                SurfaceItem::AssistantPlan {
                    id,
                    turn_id,
                    text,
                    pinned,
                } => batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response })
                            if response.plan_item.as_ref().is_some_and(|value|
                                value.id == *id
                                    && value.turn_id == *turn_id
                                    && value.text == *text
                                    && value.pinned == *pinned)
                    )
                }),
                SurfaceItem::ToolResultMessage {
                    turn_id,
                    tool_call_id,
                    content,
                    terminal,
                    ..
                } => batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Tool(ToolPatch::Completed { result })
                            if result.tool_call_id == *tool_call_id
                                && result.terminal == *terminal
                                && snapshot.tools.iter().any(|tool|
                                    tool.request.tool_call_id == *tool_call_id
                                        && tool.request.turn_id == *turn_id)
                                && result.output.as_ref().or(result.error.as_ref()) == Some(content)
                    )
                }),
                SurfaceItem::UserMessage { .. } | SurfaceItem::SystemMessage { .. } => true,
            };
            let paired = paired
                || compaction_item_replacement_paired(snapshot, batch, item)
                || manual_compaction_rebuild_batch(batch);
            if !paired {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "derived item lacks its matching typed patch",
                ));
            }
            snapshot.items.push(item.clone());
            Ok(())
        }
        ItemPatch::InputResolved { item_id, fact } => {
            let item = snapshot
                .items
                .iter_mut()
                .find(|item| surface_item_id(item) == item_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "input item does not exist",
                    )
                })?;
            let SurfaceItem::UserMessage { input, .. } = item else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "only a user input item can resolve",
                ));
            };
            if !matches!(input, SurfaceUserInputState::Pending { .. }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "input item is not pending",
                ));
            }
            *input = SurfaceUserInputState::Resolved { fact: fact.clone() };
            Ok(())
        }
        ItemPatch::InputResolutionFailed {
            item_id,
            code,
            message,
        } => {
            let item = snapshot
                .items
                .iter_mut()
                .find(|item| surface_item_id(item) == item_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "input item does not exist",
                    )
                })?;
            let SurfaceItem::UserMessage { input, .. } = item else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "only a user input item can fail resolution",
                ));
            };
            let SurfaceUserInputState::Pending {
                presentation,
                correlation_id,
            } = input
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "input item is not pending",
                ));
            };
            *input = SurfaceUserInputState::ResolutionFailed {
                presentation: presentation.clone(),
                correlation_id: correlation_id.clone(),
                code: *code,
                message: message.clone(),
            };
            Ok(())
        }
        ItemPatch::Removed { item_id, .. } => {
            let position = snapshot
                .items
                .iter()
                .position(|item| surface_item_id(item) == item_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "item does not exist",
                    )
                })?;
            snapshot.items.remove(position);
            Ok(())
        }
    }
}

fn compaction_item_replacement_paired(
    snapshot: &SurfaceSnapshot,
    batch: &SurfaceCommitBatch,
    added: &SurfaceItem,
) -> bool {
    let SurfaceItem::ToolResultMessage {
        tool_call_id: added_tool_call_id,
        ..
    } = added
    else {
        return false;
    };
    let completes_manual_compaction = batch.events.as_slice().iter().any(|event| {
        matches!(
            &event.event,
            SurfaceEvent::Context(SurfaceContextSnapshot {
                compaction: CompactionState::Completed {
                    reason: CompactionReason::Manual,
                    ..
                },
                ..
            })
        )
    });
    completes_manual_compaction
        && snapshot.items.iter().any(|item| {
            let SurfaceItem::ToolResultMessage {
                id, tool_call_id, ..
            } = item
            else {
                return false;
            };
            tool_call_id == added_tool_call_id
                && batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Item(ItemPatch::Removed {
                            item_id,
                            reason: ItemRemovalReason::Compacted,
                        }) if item_id == id
                    )
                })
        })
}

fn assistant_item_text<'a>(
    response: &'a SurfaceCompletedModelResponse,
    channel: AssistantChannel,
    item_id: &SurfaceItemId,
) -> Option<&'a DisplayText> {
    match channel {
        AssistantChannel::Message => response
            .message_item
            .as_ref()
            .filter(|item| item.id == *item_id)
            .map(|item| &item.text),
        AssistantChannel::Reasoning => response
            .reasoning_item
            .as_ref()
            .filter(|item| item.id == *item_id)
            .map(|item| &item.content),
        AssistantChannel::Plan => response
            .plan_item
            .as_ref()
            .filter(|item| item.id == *item_id)
            .map(|item| &item.text),
    }
}

fn apply_assistant_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    batch: &SurfaceCommitBatch,
    patch: &AssistantPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        AssistantPatch::StreamOpened { stream } => {
            require_event_scope_owns_generation(snapshot, envelope, &stream.fence)?;
            if stream.state != SurfaceAssistantStreamState::Open
                || stream.next_offset.get() != stream.text.as_str().len() as u64
                || snapshot
                    .assistant_streams
                    .iter()
                    .any(|value| value.stream_id == stream.stream_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "assistant stream is not a unique canonical open stream",
                ));
            }
            snapshot.assistant_streams.push(stream.clone());
            Ok(())
        }
        AssistantPatch::Delta {
            stream_id,
            offset,
            text,
        } => {
            let fence = snapshot
                .assistant_streams
                .iter()
                .find(|stream| stream.stream_id == *stream_id)
                .map(|stream| stream.fence.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "assistant stream does not exist",
                    )
                })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let stream = snapshot
                .assistant_streams
                .iter_mut()
                .find(|stream| stream.stream_id == *stream_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "assistant stream does not exist",
                    )
                })?;
            if stream.state != SurfaceAssistantStreamState::Open || stream.next_offset != *offset {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "assistant delta offset is not contiguous",
                ));
            }
            stream.text.push_str(text.as_str());
            stream.next_offset = ByteOffset::new(offset.get() + text.as_str().len() as u64);
            Ok(())
        }
        AssistantPatch::ResponseCompleted { response } => {
            let fence =
                generation_fence_for_turn(snapshot, &response.turn_id).ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "assistant response turn does not exist",
                    )
                })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            for stream in snapshot.assistant_streams.iter_mut().filter(|stream| {
                stream.turn_id == response.turn_id
                    && stream.state == SurfaceAssistantStreamState::Open
            }) {
                let Some(completed_text) =
                    assistant_item_text(response, stream.channel, &stream.item_id)
                else {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "open assistant stream lacks a completed response item",
                    ));
                };
                if &stream.text != completed_text {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "assistant stream text differs from completed response",
                    ));
                }
                stream.state = SurfaceAssistantStreamState::Completed;
            }
            for raw_call in &response.tool_calls {
                let paired = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Tool(ToolPatch::Requested { request })
                            if request.source_response_id.as_ref() == Some(&response.response_id)
                                && request.turn_id == response.turn_id
                                && request.tool_call_id == raw_call.id
                                && request.name == raw_call.name
                                && request.raw_arguments == raw_call.raw_arguments
                                && request.arguments_digest == raw_call.arguments_digest
                    )
                });
                if !paired {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "completed response tool call lacks a matching tool request",
                    ));
                }
            }
            let items = [
                response
                    .message_item
                    .as_ref()
                    .map(|item| SurfaceItem::AssistantMessage {
                        id: item.id.clone(),
                        turn_id: item.turn_id.clone(),
                        text: item.text.clone(),
                        pinned: item.pinned,
                    }),
                response
                    .reasoning_item
                    .as_ref()
                    .map(|item| SurfaceItem::AssistantReasoning {
                        id: item.id.clone(),
                        turn_id: item.turn_id.clone(),
                        summary: item.summary.clone(),
                        content: item.content.clone(),
                        pinned: item.pinned,
                    }),
                response
                    .plan_item
                    .as_ref()
                    .map(|item| SurfaceItem::AssistantPlan {
                        id: item.id.clone(),
                        turn_id: item.turn_id.clone(),
                        text: item.text.clone(),
                        pinned: item.pinned,
                    }),
            ];
            for item in items.into_iter().flatten() {
                if snapshot
                    .items
                    .iter()
                    .any(|existing| surface_item_id(existing) == surface_item_id(&item))
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::DuplicateTransition,
                        "completed assistant item already exists",
                    ));
                }
                snapshot.items.push(item);
            }
            Ok(())
        }
        AssistantPatch::StreamDiscarded { stream_id, .. } => {
            let fence = snapshot
                .assistant_streams
                .iter()
                .find(|stream| stream.stream_id == *stream_id)
                .map(|stream| stream.fence.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "assistant stream does not exist",
                    )
                })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let stream = snapshot
                .assistant_streams
                .iter_mut()
                .find(|stream| stream.stream_id == *stream_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "assistant stream does not exist",
                    )
                })?;
            if stream.state != SurfaceAssistantStreamState::Open {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "closed assistant stream is absorbing",
                ));
            }
            stream.state = SurfaceAssistantStreamState::Discarded;
            Ok(())
        }
    }
}

fn capability_state_terminal(state: &SurfaceCapabilityCallState) -> bool {
    matches!(
        state,
        SurfaceCapabilityCallState::Completed { .. }
            | SurfaceCapabilityCallState::FailedBeforeWrite { .. }
            | SurfaceCapabilityCallState::ObservationUnavailable { .. }
            | SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
    )
}

fn capability_external_effect(kind: SurfaceCapabilityCallKind) -> Option<ExternalEffectKind> {
    match kind {
        SurfaceCapabilityCallKind::WriteTextFile => Some(ExternalEffectKind::FileWrite),
        SurfaceCapabilityCallKind::TerminalCreate => Some(ExternalEffectKind::TerminalCreate),
        SurfaceCapabilityCallKind::TerminalKill => Some(ExternalEffectKind::TerminalKill),
        SurfaceCapabilityCallKind::TerminalRelease => Some(ExternalEffectKind::TerminalRelease),
        SurfaceCapabilityCallKind::ReadTextFile
        | SurfaceCapabilityCallKind::TerminalOutput
        | SurfaceCapabilityCallKind::TerminalWaitForExit => None,
    }
}

fn capability_result_matches_kind(
    kind: SurfaceCapabilityCallKind,
    result: &CapabilityCallResult,
) -> bool {
    matches!(result, CapabilityCallResult::RemoteError { .. })
        || matches!(
            (kind, result),
            (
                SurfaceCapabilityCallKind::ReadTextFile,
                CapabilityCallResult::ReadTextFile { .. }
            ) | (
                SurfaceCapabilityCallKind::WriteTextFile,
                CapabilityCallResult::WriteTextFileAcknowledged
            ) | (
                SurfaceCapabilityCallKind::TerminalCreate,
                CapabilityCallResult::TerminalCreated { .. }
            ) | (
                SurfaceCapabilityCallKind::TerminalOutput,
                CapabilityCallResult::TerminalOutputObserved { .. }
            ) | (
                SurfaceCapabilityCallKind::TerminalWaitForExit,
                CapabilityCallResult::TerminalExitObserved { .. }
            ) | (
                SurfaceCapabilityCallKind::TerminalKill,
                CapabilityCallResult::TerminalKillAcknowledged
            ) | (
                SurfaceCapabilityCallKind::TerminalRelease,
                CapabilityCallResult::TerminalReleaseAcknowledged
            )
        )
}

fn capability_transition_allowed(
    kind: SurfaceCapabilityCallKind,
    from: &SurfaceCapabilityCallState,
    to: &SurfaceCapabilityCallState,
) -> bool {
    match (from, to, capability_external_effect(kind)) {
        (
            SurfaceCapabilityCallState::Prepared,
            SurfaceCapabilityCallState::DeliveryPossible,
            Some(_),
        )
        | (
            SurfaceCapabilityCallState::Prepared,
            SurfaceCapabilityCallState::WrittenAwaitingResponse,
            None,
        )
        | (
            SurfaceCapabilityCallState::Prepared,
            SurfaceCapabilityCallState::FailedBeforeWrite { .. },
            _,
        )
        | (
            SurfaceCapabilityCallState::DeliveryPossible,
            SurfaceCapabilityCallState::WrittenAwaitingResponse,
            Some(_),
        ) => true,
        (
            SurfaceCapabilityCallState::DeliveryPossible
            | SurfaceCapabilityCallState::WrittenAwaitingResponse,
            SurfaceCapabilityCallState::ExternalEffectAmbiguous { effect_kind, .. },
            Some(expected),
        ) => *effect_kind == expected,
        (
            SurfaceCapabilityCallState::WrittenAwaitingResponse,
            SurfaceCapabilityCallState::ObservationUnavailable { .. },
            None,
        ) => true,
        (
            SurfaceCapabilityCallState::WrittenAwaitingResponse,
            SurfaceCapabilityCallState::Completed { result, .. },
            _,
        ) => capability_result_matches_kind(kind, result),
        _ => false,
    }
}

fn remote_terminal_lease_transition_allowed(
    owner_fence: &SurfaceOperationFence,
    from: Option<&SurfaceRemoteTerminalLeaseState>,
    to: &SurfaceRemoteTerminalLeaseState,
) -> bool {
    match (from, to) {
        (
            None,
            SurfaceRemoteTerminalLeaseState::Live {
                owner_fence: next_owner,
                ..
            },
        ) => next_owner == owner_fence,
        (
            None,
            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                terminal_id: Some(_),
                owner_fence: next_owner,
            },
        ) => next_owner == owner_fence,
        (None, SurfaceRemoteTerminalLeaseState::IdentityUnknown { .. }) => true,
        (
            Some(SurfaceRemoteTerminalLeaseState::Live {
                terminal_id,
                owner_fence: current_owner,
            }),
            SurfaceRemoteTerminalLeaseState::KillPending {
                terminal_id: next_terminal,
                owner_fence: next_owner,
            }
            | SurfaceRemoteTerminalLeaseState::ReleasePending {
                terminal_id: next_terminal,
                owner_fence: next_owner,
            },
        ) => {
            current_owner == owner_fence
                && next_owner == current_owner
                && next_terminal == terminal_id
        }
        (
            Some(
                SurfaceRemoteTerminalLeaseState::Live {
                    terminal_id,
                    owner_fence: current_owner,
                }
                | SurfaceRemoteTerminalLeaseState::KillPending {
                    terminal_id,
                    owner_fence: current_owner,
                }
                | SurfaceRemoteTerminalLeaseState::ReleasePending {
                    terminal_id,
                    owner_fence: current_owner,
                },
            ),
            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                terminal_id: next_terminal,
                owner_fence: next_owner,
            },
        ) => {
            current_owner == owner_fence
                && next_owner == current_owner
                && next_terminal.as_ref() == Some(terminal_id)
        }
        (
            Some(SurfaceRemoteTerminalLeaseState::KillPending {
                terminal_id,
                owner_fence: current_owner,
            }),
            SurfaceRemoteTerminalLeaseState::ReleasePending {
                terminal_id: next_terminal,
                owner_fence: next_owner,
            },
        ) => {
            current_owner == owner_fence
                && next_owner == current_owner
                && next_terminal == terminal_id
        }
        (
            Some(SurfaceRemoteTerminalLeaseState::ReleasePending {
                owner_fence: current_owner,
                ..
            }),
            SurfaceRemoteTerminalLeaseState::Released,
        ) => current_owner == owner_fence,
        _ => false,
    }
}

fn apply_tool_patch(
    state: &mut SurfaceReducerState,
    envelope: &SurfaceEventEnvelope,
    batch: &SurfaceCommitBatch,
    patch: &ToolPatch,
) -> Result<(), SurfaceReducerError> {
    let SurfaceReducerState {
        snapshot,
        live_tool_argument_progress,
        ..
    } = state;
    match patch {
        ToolPatch::Requested { request } => {
            let fence = generation_fence_for_turn(snapshot, &request.turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "tool request turn does not exist",
                )
            })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            if let Some(source_response_id) = request.source_response_id.as_ref() {
                let paired = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response })
                            if &response.response_id == source_response_id
                                && response.turn_id == request.turn_id
                                && response.tool_calls.iter().any(|raw_call| {
                                    raw_call.id == request.tool_call_id
                                        && raw_call.name == request.name
                                        && raw_call.raw_arguments == request.raw_arguments
                                        && raw_call.arguments_digest == request.arguments_digest
                                })
                    )
                });
                if !paired {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "provider tool request lacks its matching response in the same batch",
                    ));
                }
            }
            if snapshot
                .tools
                .iter()
                .any(|tool| tool.request.tool_call_id == request.tool_call_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "tool call identity already exists",
                ));
            }
            let final_arguments_bytes = ByteCount::new(request.raw_arguments.as_str().len() as u64);
            if let Some(progress) = live_tool_argument_progress.remove(&request.tool_call_id) {
                if progress.fence != fence
                    || final_arguments_bytes.get() < progress.arguments_bytes.get()
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "tool request does not consume its exact argument progress lane",
                    ));
                }
            }
            snapshot.tools.push(SurfaceToolView {
                request: request.clone(),
                state: SurfaceToolViewState::Requested,
                invocation_started: None,
                arguments_bytes: final_arguments_bytes,
                output_bytes: ByteCount::new(0),
                streamed_output: DisplayText::new(""),
                streamed_output_truncated: false,
                result: None,
                capability_calls: Vec::new(),
                terminal_leases: Vec::new(),
            });
            Ok(())
        }
        ToolPatch::ArgumentsProgress {
            tool_call_id,
            arguments_bytes,
        } => {
            let existing_turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == *tool_call_id)
                .map(|tool| tool.request.turn_id.clone());
            let fence = if let Some(turn_id) = existing_turn_id {
                generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool request turn does not exist",
                    )
                })?
            } else {
                let scoped_fence = match &envelope.scope {
                    SurfaceScope::Generation { fence } => fence.clone(),
                    SurfaceScope::Background { fence } => fence.operation_fence.clone(),
                    _ => unreachable!("tool scope class is validated before reduction"),
                };
                if snapshot_generation(snapshot, &scoped_fence).is_none() {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "pre-request tool argument progress generation does not exist",
                    ));
                }
                scoped_fence
            };
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            if let Some(tool) = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == *tool_call_id)
            {
                if tool.state == SurfaceToolViewState::Completed
                    || arguments_bytes.get() < tool.arguments_bytes.get()
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "tool argument progress regressed or followed terminal",
                    ));
                }
                tool.arguments_bytes = *arguments_bytes;
                return Ok(());
            }
            if !matches!(batch.commit_class, CommitClass::Ephemeral { .. }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::CommitClassMismatch,
                    "pre-request tool argument progress must be ephemeral",
                ));
            }
            if let Some(progress) = live_tool_argument_progress.get_mut(tool_call_id) {
                if progress.fence != fence || arguments_bytes.get() < progress.arguments_bytes.get()
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "pre-request tool argument progress changed owner or regressed",
                    ));
                }
                progress.arguments_bytes = *arguments_bytes;
            } else {
                live_tool_argument_progress.insert(
                    tool_call_id.clone(),
                    LiveToolArgumentProgress {
                        fence,
                        arguments_bytes: *arguments_bytes,
                    },
                );
            }
            Ok(())
        }
        ToolPatch::OutputDelta {
            tool_call_id,
            offset,
            chunk,
        } => {
            let turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == *tool_call_id)
                .map(|tool| tool.request.turn_id.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool call does not exist",
                    )
                })?;
            let fence = generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "tool request turn does not exist",
                )
            })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let tool = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == *tool_call_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool call does not exist",
                    )
                })?;
            if tool.state == SurfaceToolViewState::Completed
                || tool.output_bytes.get() != offset.get()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "tool output offset is not contiguous",
                ));
            }
            tool.streamed_output = DisplayText::new(format!(
                "{}{}",
                tool.streamed_output.as_str(),
                chunk.as_str()
            ));
            tool.output_bytes = ByteCount::new(offset.get() + chunk.as_str().len() as u64);
            tool.state = SurfaceToolViewState::Running;
            Ok(())
        }
        ToolPatch::InvocationStartedV1 { receipt } => {
            if !matches!(batch.commit_class, CommitClass::Recorded { .. }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::CommitClassMismatch,
                    "tool invocation start receipt must be durable",
                ));
            }
            let turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == *receipt.invocation_id())
                .map(|tool| tool.request.turn_id.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool invocation start receipt has no matching request",
                    )
                })?;
            let fence = generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "tool invocation start receipt generation does not exist",
                )
            })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let tool = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == *receipt.invocation_id())
                .expect("tool request was resolved before mutable projection update");
            if receipt.fence() != &fence
                || receipt.revision().get() != 1
                || tool.invocation_started.is_some()
                || tool.state != SurfaceToolViewState::Requested
                || tool.result.is_some()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "tool invocation start receipt is stale, duplicated, or follows execution",
                ));
            }
            tool.invocation_started = Some(receipt.clone());
            tool.state = SurfaceToolViewState::Running;
            Ok(())
        }
        ToolPatch::Completed { result } => {
            let turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == result.tool_call_id)
                .map(|tool| tool.request.turn_id.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool call does not exist",
                    )
                })?;
            let fence = generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "tool request turn does not exist",
                )
            })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let tool = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == result.tool_call_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "tool call does not exist",
                    )
                })?;
            let paired_item = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Item(ItemPatch::Added {
                        item: SurfaceItem::ToolResultMessage {
                            turn_id,
                            tool_call_id,
                            content,
                            terminal,
                            ..
                        }
                    }) if turn_id == &tool.request.turn_id
                        && tool_call_id == &result.tool_call_id
                        && terminal == &result.terminal
                        && result.output.as_ref().or(result.error.as_ref()) == Some(content)
                )
            });
            let capability_calls_settled = tool
                .capability_calls
                .iter()
                .all(|call| capability_state_terminal(&call.state));
            let terminal_valid = result.name == tool.request.name
                && tool.state != SurfaceToolViewState::Completed
                && capability_calls_settled
                && (!matches!(result.terminal.kind, SurfaceToolResultKind::Success)
                    || result.error.is_none())
                && (!matches!(
                    result.terminal.kind,
                    SurfaceToolResultKind::Denied | SurfaceToolResultKind::InvalidArguments
                ) || result.terminal.invocation_started == ToolInvocationStarted::No)
                && (result.exit_code.is_none() || tool.request.action == SurfaceToolAction::Shell)
                && (result.file_change.is_none()
                    || tool.request.action == SurfaceToolAction::Write);
            if !terminal_valid || !paired_item {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "tool terminal, capability settlement, or matching result item is invalid",
                ));
            }
            tool.state = SurfaceToolViewState::Completed;
            tool.result = Some(result.clone());
            Ok(())
        }
        ToolPatch::CapabilityCallChanged { call } => {
            let turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
                .map(|tool| tool.request.turn_id.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "owning tool call does not exist",
                    )
                })?;
            let fence = generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "owning tool call turn does not exist",
                )
            })?;
            if call.fence != fence {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::ScopeMismatch,
                    "capability call fence does not own the tool generation",
                ));
            }
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let tool = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "owning tool call does not exist",
                    )
                })?;
            if let Some(current) = tool
                .capability_calls
                .iter_mut()
                .find(|current| current.call_id == call.call_id)
            {
                if current.acp_session_id != call.acp_session_id
                    || current.fence != call.fence
                    || current.capability_revision != call.capability_revision
                    || current.policy_epoch != call.policy_epoch
                    || current.kind != call.kind
                    || current.arguments_digest != call.arguments_digest
                    || current.owning_tool_call_id != call.owning_tool_call_id
                    || capability_state_terminal(&current.state)
                    || !capability_transition_allowed(call.kind, &current.state, &call.state)
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "capability call identity changed or terminal escaped",
                    ));
                }
                current.state = call.state.clone();
            } else {
                if !matches!(call.state, SurfaceCapabilityCallState::Prepared) {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "capability call must begin prepared",
                    ));
                }
                tool.capability_calls.push(call.clone());
            }
            Ok(())
        }
        ToolPatch::RemoteTerminalLeaseChanged { lease } => {
            let turn_id = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == lease.owning_tool_call_id)
                .map(|tool| tool.request.turn_id.clone())
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "owning tool call does not exist",
                    )
                })?;
            let fence = generation_fence_for_turn(snapshot, &turn_id).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "tool request turn does not exist",
                )
            })?;
            require_event_scope_owns_generation(snapshot, envelope, &fence)?;
            let tool = snapshot
                .tools
                .iter_mut()
                .find(|tool| tool.request.tool_call_id == lease.owning_tool_call_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "owning tool call does not exist",
                    )
                })?;
            if let Some(current) = tool
                .terminal_leases
                .iter_mut()
                .find(|current| current.lease_id == lease.lease_id)
            {
                if current.owning_tool_call_id != lease.owning_tool_call_id
                    || !remote_terminal_lease_transition_allowed(
                        &fence,
                        Some(&current.state),
                        &lease.state,
                    )
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "terminal lease identity changed or terminal escaped",
                    ));
                }
                current.state = lease.state.clone();
            } else {
                if !remote_terminal_lease_transition_allowed(&fence, None, &lease.state) {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "terminal lease must begin with an owned live or degraded identity",
                    ));
                }
                tool.terminal_leases.push(lease.clone());
            }
            Ok(())
        }
    }
}

fn interaction_kind_matches_request(
    kind: SurfaceInteractionKind,
    request: &SurfaceInteractionRequest,
) -> bool {
    matches!(
        (kind, request),
        (
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionRequest::ToolApproval { .. }
        ) | (
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionRequest::PermissionRequest { .. }
        ) | (
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionRequest::UserInput { .. }
        ) | (
            SurfaceInteractionKind::McpElicitation,
            SurfaceInteractionRequest::McpElicitation { .. }
        ) | (
            SurfaceInteractionKind::BackgroundApproval,
            SurfaceInteractionRequest::BackgroundApproval { .. }
        )
    )
}

fn snapshot_operation_record<'a>(
    snapshot: &'a SurfaceSnapshot,
    operation_id: &SurfaceOperationId,
) -> Option<&'a OperationRecord> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| operation.operation_id == *operation_id)
}

fn interaction_authority_matches(
    snapshot: &SurfaceSnapshot,
    fence: &SurfaceOperationFence,
    authority: &AuthorityFingerprint,
) -> bool {
    let Some(operation) = snapshot_operation_record(snapshot, &fence.operation_id) else {
        return false;
    };
    let Some(generation) = snapshot_generation_record(snapshot, fence) else {
        return false;
    };
    let generation_policy_epoch = generation
        .started_witness
        .as_ref()
        .map_or(operation.intent.policy_epoch, |witness| {
            witness.policy_epoch
        });
    if authority.operation_id() != &fence.operation_id
        || authority.policy_epoch() != operation.intent.policy_epoch
        || authority.policy_epoch() != generation_policy_epoch
        || authority.capability_digest() != &generation.capability_fingerprint
    {
        return false;
    }
    match &generation.replayability {
        Replayability::Replayable {
            request_digest,
            cwd,
            workspace_roots,
            policy_epoch,
            tool_schema_digest,
            ..
        } => {
            request_digest.as_ref() == Some(authority.request_digest())
                && authority.cwd() == cwd
                && authority.workspace_roots_digest()
                    == &sha256(&serde_json::to_vec(workspace_roots).unwrap_or_default())
                && authority.policy_epoch() == *policy_epoch
                && authority.tool_digest() == tool_schema_digest
        }
        Replayability::NonReplayable { .. } => false,
    }
}

fn interaction_tool_authority_matches(
    tool: &SurfaceToolRequest,
    authority: &AuthorityFingerprint,
) -> bool {
    authority.executable_generation() == &sha256(&serde_json::to_vec(tool).unwrap_or_default())
        && authority.artifact_generation() == &tool.arguments_digest
}

fn interaction_tool_matches(
    snapshot: &SurfaceSnapshot,
    fence: &SurfaceOperationFence,
    requested: &SurfaceToolRequest,
) -> bool {
    snapshot.tools.iter().any(|tool| tool.request == *requested)
        && generation_fence_for_turn(snapshot, &requested.turn_id).as_ref() == Some(fence)
}

fn interaction_task_fence_matches(snapshot: &SurfaceSnapshot, fence: &SurfaceTaskFence) -> bool {
    snapshot.tasks.iter().any(|task| {
        task.task_id == fence.task_id
            && task.revision == fence.task_revision
            && task.backgrounded == fence.background_owner.is_some()
            && task.background_fence == fence.background_owner
    })
}

fn interaction_request_matches_snapshot(
    snapshot: &SurfaceSnapshot,
    interaction: &SurfaceInteractionView,
) -> bool {
    match &interaction.request {
        SurfaceInteractionRequest::ToolApproval {
            tool, authority, ..
        } => {
            interaction_tool_matches(snapshot, &interaction.fence, tool)
                && interaction_authority_matches(snapshot, &interaction.fence, authority)
                && interaction_tool_authority_matches(tool, authority)
        }
        SurfaceInteractionRequest::PermissionRequest {
            tool_call_id,
            authority,
            ..
        } => snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *tool_call_id)
            .is_some_and(|tool| {
                generation_fence_for_turn(snapshot, &tool.request.turn_id).as_ref()
                    == Some(&interaction.fence)
                    && interaction_authority_matches(snapshot, &interaction.fence, authority)
                    && interaction_tool_authority_matches(&tool.request, authority)
            }),
        SurfaceInteractionRequest::BackgroundApproval {
            task,
            tool,
            authority,
        } => {
            interaction_task_fence_matches(snapshot, task)
                && interaction_tool_matches(snapshot, &interaction.fence, tool)
                && interaction_authority_matches(snapshot, &interaction.fence, authority)
                && interaction_tool_authority_matches(tool, authority)
        }
        SurfaceInteractionRequest::UserInput { .. }
        | SurfaceInteractionRequest::McpElicitation { .. } => true,
    }
}

fn interaction_route_epoch(route: &SurfaceInteractionRoute) -> ResponseRouteEpoch {
    match route {
        SurfaceInteractionRoute::Unassigned { epoch }
        | SurfaceInteractionRoute::Exclusive { epoch, .. }
        | SurfaceInteractionRoute::SharedFirstCommitWins { epoch, .. } => *epoch,
    }
}

fn interaction_safe_projection_matches_kind(
    kind: SurfaceInteractionKind,
    projection: &SurfaceInteractionSafeProjection,
) -> bool {
    matches!(
        (kind, projection),
        (
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionSafeProjection::ToolApproval { .. }
        ) | (
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionSafeProjection::PermissionRequest { .. }
        ) | (
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionSafeProjection::UserInput { .. }
        ) | (
            SurfaceInteractionKind::McpElicitation,
            SurfaceInteractionSafeProjection::McpElicitation { .. }
        ) | (
            SurfaceInteractionKind::BackgroundApproval,
            SurfaceInteractionSafeProjection::BackgroundApproval { .. }
        )
    )
}

fn interaction_revision_is_contiguous(
    expected: InteractionRevision,
    next: InteractionRevision,
) -> bool {
    expected
        .get()
        .checked_add(1)
        .is_some_and(|expected_next| next.get() == expected_next)
}

fn interaction_cancel_reason_matches_disposition(
    disposition: &InteractionUnavailableDisposition,
    reason: &InteractionCancelReason,
) -> bool {
    match reason {
        InteractionCancelReason::OperationCancelled { .. }
        | InteractionCancelReason::HostShutdown
        | InteractionCancelReason::ThreadClose => true,
        InteractionCancelReason::CapabilityUnavailable => {
            matches!(
                disposition,
                InteractionUnavailableDisposition::FailOperation
                    | InteractionUnavailableDisposition::RestartableToolApproval { .. }
                    | InteractionUnavailableDisposition::RestartablePermissionRequest { .. }
                    | InteractionUnavailableDisposition::RestartableUserInput { .. }
                    | InteractionUnavailableDisposition::RestartableMcpElicitation { .. }
            )
        }
        InteractionCancelReason::ExpiryAuthorityUnavailable { deadline, failure } => {
            let InteractionUnavailableDisposition::AwaitCapableAttachment {
                deadline: persisted,
            } = disposition
            else {
                return false;
            };
            if persisted != deadline {
                return false;
            }
            match failure {
                InteractionExpiryAuthorityFailure::ClockIdMismatch { expected, observed } => {
                    expected == &deadline.expires_at.clock_id && expected != observed
                }
                InteractionExpiryAuthorityFailure::TickArithmeticOverflow { clock_id } => {
                    clock_id == &deadline.expires_at.clock_id
                }
                InteractionExpiryAuthorityFailure::IssuingHostLost {
                    clock_id,
                    issuing_host_incarnation,
                } => {
                    clock_id == &deadline.expires_at.clock_id
                        && issuing_host_incarnation == &deadline.issuing_host_incarnation
                }
            }
        }
    }
}

fn apply_interaction_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &InteractionPatch,
) -> Result<(), SurfaceReducerError> {
    if let InteractionPatch::Requested { interaction } = patch {
        require_event_scope_owns_generation(snapshot, envelope, &interaction.fence)?;
        if interaction.revision.get() != 1
            || !matches!(
                interaction.lifecycle,
                SurfaceInteractionLifecycle::Requested
            )
            || !interaction_kind_matches_request(interaction.kind, &interaction.request)
            || !interaction_request_matches_snapshot(snapshot, interaction)
            || snapshot
                .interactions
                .iter()
                .any(|value| value.interaction_id == interaction.interaction_id)
        {
            return Err(event_error(
                envelope,
                SurfaceReducerErrorCode::IllegalTransition,
                "interaction request is not canonical or unique",
            ));
        }
        snapshot.interactions.push(interaction.clone());
        return Ok(());
    }
    let (interaction_id, expected_revision, next_revision) = match patch {
        InteractionPatch::Requested { .. } => unreachable!(),
        InteractionPatch::RouteChanged {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::Resolved {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::ContinuationDispatchStarted {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::ContinuationDispatchConsumed {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::Cancelled {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::Expired {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        }
        | InteractionPatch::Transferred {
            interaction_id,
            expected_revision,
            next_revision,
            ..
        } => (interaction_id, *expected_revision, *next_revision),
    };
    let interaction_position = snapshot
        .interactions
        .iter()
        .position(|interaction| interaction.interaction_id == *interaction_id)
        .ok_or_else(|| {
            event_error(
                envelope,
                SurfaceReducerErrorCode::MissingIdentity,
                "interaction does not exist",
            )
        })?;
    let current = &snapshot.interactions[interaction_position];
    let scope_matches = match patch {
        InteractionPatch::Transferred {
            background_fence, ..
        } => {
            background_fence.operation_fence == current.fence
                && event_scope_owns_generation(snapshot, envelope, &current.fence)
        }
        InteractionPatch::ContinuationDispatchStarted { .. }
        | InteractionPatch::ContinuationDispatchConsumed { .. } => {
            matches!(envelope.scope, SurfaceScope::Thread)
        }
        _ => match &current.lifecycle {
            SurfaceInteractionLifecycle::Transferred { background_fence } => matches!(
                &envelope.scope,
                SurfaceScope::Background { fence } if fence == background_fence
            ),
            _ => event_scope_owns_generation(snapshot, envelope, &current.fence),
        },
    };
    if !scope_matches {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::ScopeMismatch,
            "interaction owner fence is stale",
        ));
    }
    let interaction = &mut snapshot.interactions[interaction_position];
    if interaction.revision != expected_revision
        || !interaction_revision_is_contiguous(expected_revision, next_revision)
    {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::StaleRevision,
            "interaction revision is not contiguous",
        ));
    }
    let open = match patch {
        InteractionPatch::ContinuationDispatchStarted { .. }
        | InteractionPatch::ContinuationDispatchConsumed { .. } => matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Resolved { .. }
        ),
        _ => matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Requested
                | SurfaceInteractionLifecycle::Transferred { .. }
        ),
    };
    if !open {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::IllegalTransition,
            "terminal interaction is absorbing",
        ));
    }
    match patch {
        InteractionPatch::RouteChanged { route, .. } => {
            let current_epoch = interaction_route_epoch(&interaction.route);
            if !current_epoch
                .get()
                .checked_add(1)
                .is_some_and(|expected| interaction_route_epoch(route).get() == expected)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "interaction route epoch is not contiguous",
                ));
            }
            interaction.route = route.clone();
        }
        InteractionPatch::Resolved {
            receipt,
            continuation,
            ..
        } => {
            if receipt.kind != interaction.kind
                || !interaction_safe_projection_matches_kind(receipt.kind, &receipt.safe_projection)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "interaction receipt kind or safe projection is invalid",
                ));
            }
            if let Some(continuation) = continuation {
                let capsule = interaction
                    .recovery_disposition
                    .restartable_continuation_turn_capsule()
                    .ok()
                    .flatten();
                if capsule
                    .as_ref()
                    .is_none_or(|capsule| continuation.validate(capsule, receipt).is_err())
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "interaction continuation answer does not match its durable request",
                    ));
                }
            }
            interaction.lifecycle = SurfaceInteractionLifecycle::Resolved {
                receipt: receipt.clone(),
            }
        }
        InteractionPatch::ContinuationDispatchStarted {
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
            ..
        } => {
            let identity = match &interaction.lifecycle {
                SurfaceInteractionLifecycle::Resolved { receipt }
                    if &receipt.receipt_id == receipt_id =>
                {
                    DurableInteractionContinuationOperationIdentity::try_new(
                        &interaction.interaction_id,
                        receipt,
                    )
                    .ok()
                }
                _ => None,
            };
            if identity.as_ref().is_none_or(|identity| {
                identity.dispatch_id() != dispatch_id
                    || identity.operation_id() != operation_id
                    || identity.turn_id() != turn_id
            }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "continuation dispatch start does not match the stable resolution identity",
                ));
            }
        }
        InteractionPatch::ContinuationDispatchConsumed {
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
            ..
        } => {
            let identity = match &interaction.lifecycle {
                SurfaceInteractionLifecycle::Resolved { receipt }
                    if &receipt.receipt_id == receipt_id =>
                {
                    DurableInteractionContinuationOperationIdentity::try_new(
                        &interaction.interaction_id,
                        receipt,
                    )
                    .ok()
                }
                _ => None,
            };
            if identity.as_ref().is_none_or(|identity| {
                identity.dispatch_id() != dispatch_id
                    || identity.operation_id() != operation_id
                    || identity.turn_id() != turn_id
            }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "continuation dispatch consumption does not match the stable resolution identity",
                ));
            }
            let operation_has_taken_over = snapshot
                .foreground_operation
                .iter()
                .chain(snapshot.queued_operations.iter())
                .chain(snapshot.operation_history.iter())
                .find(|operation| &operation.operation_id == operation_id)
                .is_some_and(|operation| {
                    operation.agent_loop_turns.iter().any(|turn| {
                        &turn.turn_id == turn_id && turn.fence.operation_id == *operation_id
                    }) || operation.terminal.is_some()
                });
            if !operation_has_taken_over {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "continuation dispatch consumption precedes operation takeover",
                ));
            }
        }
        InteractionPatch::Cancelled { reason, .. } => {
            if !interaction_cancel_reason_matches_disposition(
                &interaction.recovery_disposition,
                reason,
            ) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "interaction cancellation reason does not match its recovery disposition",
                ));
            }
            interaction.lifecycle = SurfaceInteractionLifecycle::Cancelled {
                reason: reason.clone(),
            }
        }
        InteractionPatch::Expired { deadline, .. } => {
            if !matches!(
                &interaction.recovery_disposition,
                InteractionUnavailableDisposition::AwaitCapableAttachment {
                    deadline: persisted,
                } if persisted == deadline
            ) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "interaction expiry lacks its persisted deadline",
                ));
            }
            interaction.lifecycle = SurfaceInteractionLifecycle::Expired {
                deadline: deadline.clone(),
            }
        }
        InteractionPatch::Transferred {
            background_fence,
            route,
            ..
        } => {
            if background_fence.operation_fence != interaction.fence {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::ScopeMismatch,
                    "interaction transfer fence differs",
                ));
            }
            let current_epoch = interaction_route_epoch(&interaction.route);
            if !current_epoch
                .get()
                .checked_add(1)
                .is_some_and(|expected| interaction_route_epoch(route).get() == expected)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "interaction transfer route epoch is not contiguous",
                ));
            }
            interaction.route = route.clone();
            interaction.lifecycle = SurfaceInteractionLifecycle::Transferred {
                background_fence: background_fence.clone(),
            };
        }
        InteractionPatch::Requested { .. } => unreachable!(),
    }
    interaction.revision = next_revision;
    Ok(())
}

fn operation_position(
    snapshot: &SurfaceSnapshot,
    operation_id: &SurfaceOperationId,
) -> Option<usize> {
    snapshot
        .queued_operations
        .iter()
        .position(|operation| operation.operation_id == *operation_id)
}

fn foreground_operation_mut<'a>(
    snapshot: &'a mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    operation_id: &SurfaceOperationId,
) -> Result<&'a mut OperationRecord, SurfaceReducerError> {
    snapshot
        .foreground_operation
        .as_mut()
        .filter(|operation| operation.operation_id == *operation_id)
        .ok_or_else(|| {
            event_error(
                envelope,
                SurfaceReducerErrorCode::MissingIdentity,
                "foreground operation does not exist",
            )
        })
}

fn operation_record_mut<'a>(
    snapshot: &'a mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    operation_id: &SurfaceOperationId,
) -> Result<&'a mut OperationRecord, SurfaceReducerError> {
    if let Some(position) = operation_position(snapshot, operation_id) {
        return Ok(&mut snapshot.queued_operations[position]);
    }
    if snapshot
        .foreground_operation
        .as_ref()
        .is_some_and(|operation| operation.operation_id == *operation_id)
    {
        return Ok(snapshot.foreground_operation.as_mut().unwrap());
    }
    snapshot
        .operation_history
        .iter_mut()
        .find(|operation| operation.operation_id == *operation_id && operation.terminal.is_none())
        .ok_or_else(|| {
            event_error(
                envelope,
                SurfaceReducerErrorCode::MissingIdentity,
                "active operation does not exist",
            )
        })
}

fn ensure_background_owner(
    snapshot: &SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    operation_id: &SurfaceOperationId,
) -> Result<(), SurfaceReducerError> {
    let background = snapshot
        .background_operations
        .iter()
        .find(|operation| operation.operation_id == *operation_id);
    match (&envelope.scope, background) {
        (SurfaceScope::Background { fence }, Some(background)) if fence == &background.fence => {
            Ok(())
        }
        (SurfaceScope::Background { .. }, _) | (_, Some(_)) => Err(event_error(
            envelope,
            SurfaceReducerErrorCode::ScopeMismatch,
            "background operation owner fence is missing or stale",
        )),
        (_, None) => Ok(()),
    }
}

fn generation_mut<'a>(
    snapshot: &'a mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    fence: &SurfaceOperationFence,
) -> Result<&'a mut GenerationRecord, SurfaceReducerError> {
    let operation = if snapshot
        .foreground_operation
        .as_ref()
        .is_some_and(|operation| operation.operation_id == fence.operation_id)
    {
        snapshot.foreground_operation.as_mut().unwrap()
    } else if let Some(operation) = snapshot.operation_history.iter_mut().find(|operation| {
        operation.operation_id == fence.operation_id && operation.terminal.is_none()
    }) {
        operation
    } else {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::MissingIdentity,
            "active operation does not exist",
        ));
    };
    operation
        .generations
        .iter_mut()
        .find(|generation| generation.fence == *fence)
        .ok_or_else(|| {
            event_error(
                envelope,
                SurfaceReducerErrorCode::MissingIdentity,
                "generation fence does not exist",
            )
        })
}

fn operation_terminal_state_is_consistent(operation: &OperationRecord) -> bool {
    matches!(operation.phase, OperationPhase::Terminal) == operation.terminal.is_some()
}

fn snapshot_operation_terminals_are_consistent(snapshot: &SurfaceSnapshot) -> bool {
    snapshot
        .queued_operations
        .iter()
        .chain(snapshot.foreground_operation.iter())
        .chain(snapshot.operation_history.iter())
        .all(operation_terminal_state_is_consistent)
}

fn generation_input_item_id(input: &GenerationInputState) -> Option<&SurfaceItemId> {
    match input {
        GenerationInputState::Pending { input_item_id, .. }
        | GenerationInputState::Resolved { input_item_id, .. }
        | GenerationInputState::Failed { input_item_id, .. } => Some(input_item_id),
        GenerationInputState::NotApplicable => None,
    }
}

fn goal_generation_identity_matches(
    goal: Option<&SurfaceGoal>,
    operation: &OperationRecord,
    generation: &GenerationRecord,
) -> bool {
    let OperationKind::GoalRun {
        goal_id,
        goal_run_id,
        initial_objective_revision,
    } = &operation.intent.kind
    else {
        return generation.goal_identity.is_none();
    };
    let Some(identity) = generation.goal_identity.as_ref() else {
        return false;
    };
    let objective_matches = if generation.fence.generation_id.get() == 0 {
        identity.objective_revision == *initial_objective_revision
    } else {
        goal.is_none_or(|goal| identity.objective_revision == goal.objective_revision)
    };
    let run_matches = goal.is_none_or(|goal| {
        goal.goal_id == *goal_id
            && goal.current_run.as_ref().is_some_and(|run| {
                run.goal_run_id == *goal_run_id && run.operation_id == operation.operation_id
            })
    });
    identity.goal_id == *goal_id
        && identity.goal_run_id == *goal_run_id
        && identity.operation_fence == generation.fence
        && identity.logical_turn_id == generation.logical_turn_id
        && generation_input_item_id(&generation.input) == Some(&identity.canonical_input_item_id)
        && identity.predecessor_fence == generation.predecessor
        && identity.attempt == generation.attempt
        && objective_matches
        && run_matches
        && identity.outer_turn_count > 0
}

fn apply_operation_patch(
    state: &mut SurfaceReducerState,
    envelope: &SurfaceEventEnvelope,
    batch: &SurfaceCommitBatch,
    patch: &OperationPatch,
) -> Result<(), SurfaceReducerError> {
    let SurfaceReducerState {
        snapshot,
        applied_control_intents,
        degraded_finalizations,
        ..
    } = state;
    if !snapshot_operation_terminals_are_consistent(snapshot) {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::IllegalTransition,
            "operation phase and terminal record are inconsistent",
        ));
    }
    match patch {
        OperationPatch::Requested { operation } => {
            let exists = snapshot
                .foreground_operation
                .iter()
                .chain(snapshot.queued_operations.iter())
                .chain(snapshot.operation_history.iter())
                .any(|existing| existing.operation_id == operation.operation_id);
            if exists {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "operation identity already exists",
                ));
            }
            if !matches!(operation.phase, OperationPhase::Requested)
                || !operation.generations.is_empty()
                || operation.finalization.is_some()
                || operation.terminal.is_some()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "requested operation is not canonical",
                ));
            }
            snapshot.queued_operations.push(operation.clone());
            snapshot
                .queued_operations
                .sort_by_key(|queued| queued.reservation.reservation_sequence);
            Ok(())
        }
        OperationPatch::ReservationQueueChanged {
            operation_id,
            reservation_sequence,
            ready_for_admission,
            queue_position,
        } => {
            let Some(position) = operation_position(snapshot, operation_id) else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "queued operation does not exist",
                ));
            };
            if !matches!(
                snapshot.queued_operations[position].phase,
                OperationPhase::Requested
            ) || *queue_position as usize >= snapshot.queued_operations.len()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "reservation queue position is invalid",
                ));
            }
            let mut operation = snapshot.queued_operations.remove(position);
            operation.reservation.reservation_sequence = *reservation_sequence;
            operation.ready_for_admission = *ready_for_admission;
            snapshot
                .queued_operations
                .insert(*queue_position as usize, operation);
            Ok(())
        }
        OperationPatch::Admitted {
            operation_id,
            logical_turn_id,
            input,
            first_generation,
        } => {
            if snapshot.foreground_operation.is_some() {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "another foreground operation is already admitted",
                ));
            }
            let Some(position) = operation_position(snapshot, operation_id) else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "queued operation does not exist",
                ));
            };
            let operation = &snapshot.queued_operations[position];
            let expected_input = match input {
                AdmittedInput::NotApplicable => GenerationInputState::NotApplicable,
                AdmittedInput::PendingUser {
                    item_id,
                    presentation,
                    correlation_id,
                } => GenerationInputState::Pending {
                    input_item_id: item_id.clone(),
                    presentation: presentation.clone(),
                    correlation_id: correlation_id.clone(),
                },
            };
            if !matches!(operation.phase, OperationPhase::Requested)
                || first_generation.fence.operation_id != *operation_id
                || first_generation.fence.thread_id != snapshot.thread.thread_id
                || first_generation.fence.thread_owner_epoch != snapshot.thread.owner_epoch
                || first_generation.fence.generation_id.get() != 0
                || first_generation.logical_turn_id != *logical_turn_id
                || first_generation.phase != GenerationPhase::Reserved
                || first_generation.input != expected_input
                || first_generation.replayability != operation.intent.initial_replayability
                || first_generation.required_capabilities != operation.intent.required_capabilities
                || first_generation.capability_fingerprint
                    != operation.intent.capability_fingerprint
                || !goal_generation_identity_matches(
                    snapshot.goal.as_ref(),
                    operation,
                    first_generation,
                )
                || first_generation.started_witness.is_some()
                || first_generation.stop_reason.is_some()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "operation admission identity or initial generation is invalid",
                ));
            }
            let mut operation = snapshot.queued_operations.remove(position);
            operation.phase = OperationPhase::Admitted;
            operation.initial_logical_turn_id = Some(logical_turn_id.clone());
            operation.initial_input_item_id = match input {
                AdmittedInput::NotApplicable => None,
                AdmittedInput::PendingUser { item_id, .. } => Some(item_id.clone()),
            };
            operation.generations.push(first_generation.clone());
            snapshot.foreground_operation = Some(operation);
            Ok(())
        }
        OperationPatch::InputBindingsResolved {
            fence,
            input_item_id,
            fact,
        } => {
            let paired = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Item(ItemPatch::InputResolved {
                        item_id,
                        fact: item_fact,
                    }) if item_id == input_item_id && item_fact == fact
                )
            });
            let generation = generation_mut(snapshot, envelope, fence)?;
            if generation.phase != GenerationPhase::Started || !paired {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "input resolution lacks a started generation or matching item patch",
                ));
            }
            let GenerationInputState::Pending {
                input_item_id: pending_item,
                ..
            } = &generation.input
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "generation input is not pending",
                ));
            };
            if pending_item != input_item_id {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "input resolution item differs from generation gate",
                ));
            }
            generation.input = GenerationInputState::Resolved {
                input_item_id: input_item_id.clone(),
                fact: fact.clone(),
            };
            Ok(())
        }
        OperationPatch::InputBindingsFailed {
            fence,
            input_item_id,
            code,
            message,
        } => {
            let paired_item = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Item(ItemPatch::InputResolutionFailed {
                        item_id,
                        code: item_code,
                        message: item_message,
                    }) if item_id == input_item_id && item_code == code && item_message == message
                )
            });
            let paired_stop = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                        fence: stopped_fence,
                        reason: GenerationStopReason::ExecutionFailed {
                            class: GenerationExecutionFailureClass::InputResolution,
                            message: stopped_message,
                        },
                        usage_delta,
                    }) if stopped_fence == fence
                        && stopped_message == message
                        && usage_delta.input_tokens == 0
                        && usage_delta.output_tokens == 0
                        && usage_delta.cache_tokens == 0
                        && usage_delta.estimated_cost_usd_micros == 0
                )
            });
            let paired_finalization = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                        operation_id,
                        ..
                    }) if operation_id == &fence.operation_id
                )
            });
            let generation = generation_mut(snapshot, envelope, fence)?;
            if generation.phase != GenerationPhase::Started
                || !paired_item
                || !paired_stop
                || !paired_finalization
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "input failure batch is not crash-complete",
                ));
            }
            if !matches!(
                &generation.input,
                GenerationInputState::Pending { input_item_id: pending, .. } if pending == input_item_id
            ) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "generation input is not the matching pending item",
                ));
            }
            generation.input = GenerationInputState::Failed {
                input_item_id: input_item_id.clone(),
                code: *code,
            };
            Ok(())
        }
        OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let identity_matches = operation.request_id == *request_id
                && match intent {
                    PendingControlIntent::Interrupt { generation_fence }
                    | PendingControlIntent::ResumeStarting { generation_fence }
                    | PendingControlIntent::ResumeAfterInterruptedStop { generation_fence } => {
                        generation_fence.operation_id == *operation_id
                            && operation
                                .generations
                                .iter()
                                .any(|generation| generation.fence == *generation_fence)
                    }
                    PendingControlIntent::Terminalize {
                        operation_id: intent_operation,
                        ..
                    }
                    | PendingControlIntent::BackgroundOnStart {
                        operation_id: intent_operation,
                        ..
                    } => intent_operation == operation_id,
                };
            let replaces_pending = matches!(
                (&operation.pending_control, intent),
                (
                    Some(PendingControlIntent::Interrupt {
                        generation_fence: interrupted,
                    }),
                    PendingControlIntent::ResumeAfterInterruptedStop {
                        generation_fence: resumed,
                    },
                ) if interrupted == resumed
            ) || matches!(
                (&operation.pending_control, intent),
                (
                    Some(PendingControlIntent::ResumeAfterInterruptedStop {
                        generation_fence: interrupted,
                    }),
                    PendingControlIntent::ResumeStarting {
                        generation_fence: successor,
                    },
                ) if successor.operation_id == interrupted.operation_id
                    && successor.generation_id.get()
                        == interrupted.generation_id.get().saturating_add(1)
                    && operation.generations.last().is_some_and(|generation| {
                        generation.fence == *successor
                            && generation.predecessor.as_ref() == Some(interrupted)
                    })
            );
            if !identity_matches || (operation.pending_control.is_some() && !replaces_pending) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "control intent identity is invalid or another intent is pending",
                ));
            }
            operation.pending_control = Some(intent.clone());
            applied_control_intents.push(AppliedControlIntentRecord {
                operation_id: operation_id.clone(),
                intent: intent.clone(),
                event_id: envelope.event_id.clone(),
                cursor: batch.cursor_after.clone(),
                commit_class: batch.commit_class.clone(),
            });
            Ok(())
        }
        OperationPatch::GenerationReserved { generation } => {
            ensure_background_owner(snapshot, envelope, &generation.fence.operation_id)?;
            let resumed_background_fence = match &envelope.scope {
                SurfaceScope::Background { fence } => Some(fence.clone()),
                _ => None,
            };
            let thread_id = snapshot.thread.thread_id.clone();
            let owner_epoch = snapshot.thread.owner_epoch;
            let goal = snapshot.goal.clone();
            let operation =
                operation_record_mut(snapshot, envelope, &generation.fence.operation_id)?;
            let next_generation_id = operation.generations.last().map_or(Some(0), |previous| {
                previous.fence.generation_id.get().checked_add(1)
            });
            let predecessor_matches = operation.generations.last().is_some_and(|previous| {
                generation.predecessor.as_ref() == Some(&previous.fence)
                    && previous.phase == GenerationPhase::Stopped
            });
            if !matches!(
                operation.phase,
                OperationPhase::Admitted | OperationPhase::Suspended { .. }
            ) || generation.fence.thread_id != thread_id
                || generation.fence.thread_owner_epoch != owner_epoch
                || Some(generation.fence.generation_id.get()) != next_generation_id
                || generation.phase != GenerationPhase::Reserved
                || generation.started_witness.is_some()
                || generation.stop_reason.is_some()
                || !predecessor_matches
                || !goal_generation_identity_matches(goal.as_ref(), operation, generation)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "reserved generation identity or phase is invalid",
                ));
            }
            operation.generations.push(generation.clone());
            if let Some(background_fence) = resumed_background_fence {
                if snapshot.foreground_operation.is_some() {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "background resume cannot replace an active foreground operation",
                    ));
                }
                let background_position = snapshot
                    .background_operations
                    .iter()
                    .position(|background| background.fence == background_fence)
                    .ok_or_else(|| {
                        event_error(
                            envelope,
                            SurfaceReducerErrorCode::MissingIdentity,
                            "background resume owner disappeared",
                        )
                    })?;
                let operation_position = snapshot
                    .operation_history
                    .iter()
                    .position(|operation| operation.operation_id == generation.fence.operation_id)
                    .ok_or_else(|| {
                        event_error(
                            envelope,
                            SurfaceReducerErrorCode::MissingIdentity,
                            "background resume operation disappeared",
                        )
                    })?;
                snapshot.background_operations.remove(background_position);
                snapshot.foreground_operation =
                    Some(snapshot.operation_history.remove(operation_position));
            }
            Ok(())
        }
        OperationPatch::GenerationStarted { fence, witness } => {
            let operation = foreground_operation_mut(snapshot, envelope, &fence.operation_id)?;
            let suspended = matches!(operation.phase, OperationPhase::Suspended { .. });
            if !matches!(
                operation.phase,
                OperationPhase::Admitted | OperationPhase::Suspended { .. }
            ) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "operation cannot start a generation from its current phase",
                ));
            }
            let generation = operation
                .generations
                .iter_mut()
                .find(|generation| generation.fence == *fence)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "generation fence does not exist",
                    )
                })?;
            if generation.phase != GenerationPhase::Reserved
                || generation.started_witness.is_some()
                || generation.stop_reason.is_some()
                || witness.started_commit_id != *commit_id(&batch.commit_class)
                || witness.settings_revision != operation.intent.settings_revision
                || witness.policy_epoch != operation.intent.policy_epoch
                || witness.durable_replayability_digest
                    != canonical_replayability_digest(&generation.replayability)
                || witness.capability_fingerprint != generation.capability_fingerprint
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "generation start witness or phase is invalid",
                ));
            }
            if suspended {
                if !matches!(
                    operation.pending_control,
                    Some(PendingControlIntent::ResumeStarting {
                        ref generation_fence
                    }) if generation_fence == fence
                ) {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "suspended operation lacks the matching resume intent",
                    ));
                }
                operation.phase = OperationPhase::Admitted;
                operation.pending_control = None;
            }
            generation.phase = GenerationPhase::Started;
            generation.started_witness = Some(witness.clone());
            Ok(())
        }
        OperationPatch::AgentLoopTurnStarted { turn } => {
            ensure_background_owner(snapshot, envelope, &turn.fence.operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, &turn.fence.operation_id)?;
            let generation = operation
                .generations
                .iter()
                .find(|generation| generation.fence == turn.fence)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "agent-loop generation does not exist",
                    )
                })?;
            let next_ordinal = operation
                .agent_loop_turns
                .iter()
                .filter(|existing| existing.fence == turn.fence)
                .count() as u32;
            if generation.phase != GenerationPhase::Started
                || !matches!(
                    generation.input,
                    GenerationInputState::NotApplicable | GenerationInputState::Resolved { .. }
                )
                || turn.ordinal != next_ordinal
                || operation.agent_loop_turns.iter().any(|existing| {
                    existing.fence == turn.fence && existing.turn_id == turn.turn_id
                })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "agent-loop turn is not the next executable turn",
                ));
            }
            operation.agent_loop_turns.push(turn.clone());
            Ok(())
        }
        OperationPatch::ModelRouteSelected { fence, .. } => {
            let generation = generation_mut(snapshot, envelope, fence)?;
            if generation.phase != GenerationPhase::Started {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "model route requires a started generation",
                ));
            }
            Ok(())
        }
        OperationPatch::VerificationStarted {
            fence,
            verification_id,
            command,
        } => {
            let generation = generation_mut(snapshot, envelope, fence)?;
            let completion_matches = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Operation(OperationPatch::VerificationCompleted {
                        fence: completed_fence,
                        verification_id: completed_id,
                        result,
                    }) if completed_fence == fence
                        && completed_id == verification_id
                        && &result.command == command
                )
            });
            if generation.phase != GenerationPhase::Started || !completion_matches {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "verification start lacks its same-batch completion",
                ));
            }
            Ok(())
        }
        OperationPatch::VerificationCompleted {
            fence,
            verification_id,
            result,
        } => {
            let generation = generation_mut(snapshot, envelope, fence)?;
            let start_matches = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    SurfaceEvent::Operation(OperationPatch::VerificationStarted {
                        fence: started_fence,
                        verification_id: started_id,
                        command,
                    }) if started_fence == fence
                        && started_id == verification_id
                        && command == &result.command
                )
            });
            if generation.phase != GenerationPhase::Started || !start_matches {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    "verification completion lacks its same-batch start",
                ));
            }
            result.validate().map_err(|error| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::InvalidOrdering,
                    format!("verification result exceeds completion proof bounds: {error}"),
                )
            })?;
            Ok(())
        }
        OperationPatch::GenerationStopped {
            fence,
            reason,
            usage_delta: _,
        } => {
            if let Some(background) = snapshot
                .background_operations
                .iter()
                .find(|operation| operation.operation_id == fence.operation_id)
            {
                if !matches!(
                    &envelope.scope,
                    SurfaceScope::Background { fence: scoped } if scoped == &background.fence
                ) {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::ScopeMismatch,
                        "background generation owner fence is stale",
                    ));
                }
            }
            let generation = generation_mut(snapshot, envelope, fence)?;
            let allowed = match generation.phase {
                GenerationPhase::Reserved => {
                    matches!(reason, GenerationStopReason::NotStarted { .. })
                }
                GenerationPhase::Started | GenerationPhase::Transferred => {
                    !matches!(reason, GenerationStopReason::NotStarted { .. })
                }
                GenerationPhase::Stopped => false,
            };
            if !allowed {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "generation stop is not allowed from its current phase",
                ));
            }
            generation.phase = GenerationPhase::Stopped;
            generation.stop_reason = Some(reason.clone());
            Ok(())
        }
        OperationPatch::GenerationTransferred {
            fence,
            background_fence,
            task_id,
        } => {
            if background_fence.operation_fence != *fence
                || snapshot
                    .background_operations
                    .iter()
                    .any(|operation| operation.operation_id == fence.operation_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "background transfer fence is invalid or already active",
                ));
            }
            let operation = foreground_operation_mut(snapshot, envelope, &fence.operation_id)?;
            if !matches!(operation.phase, OperationPhase::Admitted) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "only an admitted operation can transfer",
                ));
            }
            let generation = operation
                .generations
                .iter_mut()
                .find(|generation| generation.fence == *fence)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "generation fence does not exist",
                    )
                })?;
            if generation.phase != GenerationPhase::Started {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "only a started generation can transfer",
                ));
            }
            generation.phase = GenerationPhase::Transferred;
            let operation = snapshot.foreground_operation.take().unwrap();
            snapshot.operation_history.push(operation);
            snapshot
                .background_operations
                .push(SurfaceBackgroundOperation {
                    operation_id: fence.operation_id.clone(),
                    fence: background_fence.clone(),
                    task_id: task_id.clone(),
                    transferred_at: batch.cursor_after.clone(),
                    finalizing_degraded: false,
                });
            Ok(())
        }
        OperationPatch::Suspended {
            operation_id,
            cause,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let generation_id = match cause {
                SuspensionCause::Interrupted { generation_id }
                | SuspensionCause::RecoveryRequired { generation_id }
                | SuspensionCause::ProviderSuspended { generation_id } => generation_id,
            };
            let generation = operation
                .generations
                .iter()
                .find(|generation| generation.fence.generation_id == *generation_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "suspension generation does not exist",
                    )
                })?;
            let reason_matches = matches!(
                (cause, generation.stop_reason.as_ref()),
                (
                    SuspensionCause::Interrupted { .. },
                    Some(GenerationStopReason::InterruptedResumable)
                ) | (
                    SuspensionCause::RecoveryRequired { .. },
                    Some(
                        GenerationStopReason::RuntimeRestart
                            | GenerationStopReason::NotStarted {
                                reason: NotStartedReason::RuntimeRestart,
                            },
                    )
                ) | (
                    SuspensionCause::ProviderSuspended { .. },
                    Some(GenerationStopReason::ProviderSuspended)
                )
            );
            if !matches!(operation.phase, OperationPhase::Admitted)
                || generation.phase != GenerationPhase::Stopped
                || !reason_matches
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "operation cannot suspend without its exact stopped generation",
                ));
            }
            if matches!(cause, SuspensionCause::Interrupted { .. }) {
                let plain_interrupt = matches!(
                    &operation.pending_control,
                    Some(PendingControlIntent::Interrupt { generation_fence })
                        if generation_fence == &generation.fence
                );
                let queued_resume = matches!(
                    &operation.pending_control,
                    Some(PendingControlIntent::ResumeAfterInterruptedStop {
                        generation_fence,
                    }) if generation_fence == &generation.fence
                );
                if !plain_interrupt && !queued_resume {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "interrupted suspension lacks the matching durable control intent",
                    ));
                }
                if plain_interrupt {
                    operation.pending_control = None;
                }
            }
            operation.phase = OperationPhase::Suspended {
                cause: cause.clone(),
            };
            Ok(())
        }
        OperationPatch::SuspensionRebasedAfterUnstartedResume {
            operation_id,
            previous_cause,
            replacement_fence,
            rebased_cause,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let current_cause_matches = matches!(
                &operation.phase,
                OperationPhase::Suspended { cause } if cause == previous_cause
            );
            let pending_matches = matches!(
                &operation.pending_control,
                Some(PendingControlIntent::ResumeStarting { generation_fence })
                    if generation_fence == replacement_fence
            );
            let replacement = operation
                .generations
                .iter()
                .find(|generation| generation.fence == *replacement_fence)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "resume replacement generation does not exist",
                    )
                })?;
            let stopped_unstarted = matches!(
                replacement.stop_reason,
                Some(GenerationStopReason::NotStarted {
                    reason: NotStartedReason::Interrupted | NotStartedReason::RuntimeRestart
                })
            );
            let rebased_generation = match rebased_cause {
                SuspensionCause::Interrupted { generation_id }
                | SuspensionCause::RecoveryRequired { generation_id }
                | SuspensionCause::ProviderSuspended { generation_id } => generation_id,
            };
            if !current_cause_matches
                || !pending_matches
                || replacement.phase != GenerationPhase::Stopped
                || !stopped_unstarted
                || *rebased_generation != replacement_fence.generation_id
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "suspension rebase witnesses do not match",
                ));
            }
            operation.phase = OperationPhase::Suspended {
                cause: rebased_cause.clone(),
            };
            operation.pending_control = None;
            Ok(())
        }
        OperationPatch::RecoveryRequired {
            operation_id,
            last_generation,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let generation = operation.generations.last().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "recovery operation has no generation",
                )
            })?;
            if generation.fence.generation_id != *last_generation
                || generation.phase != GenerationPhase::Stopped
                || !matches!(
                    generation.stop_reason,
                    Some(GenerationStopReason::RuntimeRestart)
                )
                || !matches!(operation.phase, OperationPhase::Admitted)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "recovery witness is not the stopped last generation",
                ));
            }
            operation.phase = OperationPhase::Suspended {
                cause: SuspensionCause::RecoveryRequired {
                    generation_id: *last_generation,
                },
            };
            Ok(())
        }
        OperationPatch::FinalizationStarted {
            operation_id,
            finalize_intent_id,
            terminal_commit_id,
            selected_cause,
            suspended_cause,
            expected_settlements,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let phase_allowed = match operation.phase {
                OperationPhase::Requested => operation.generations.is_empty(),
                OperationPhase::Admitted => operation
                    .generations
                    .iter()
                    .all(|generation| generation.phase == GenerationPhase::Stopped),
                OperationPhase::Suspended { .. } => true,
                OperationPhase::Finalizing { .. }
                | OperationPhase::FinalizingDegraded { .. }
                | OperationPhase::Terminal => false,
            };
            let suspended_matches = match (&operation.phase, selected_cause, suspended_cause) {
                (
                    OperationPhase::Suspended { .. },
                    OperationFinalizationCause::Suspended(selected),
                    Some(suspended),
                ) => selected == suspended,
                (OperationPhase::Requested | OperationPhase::Admitted, selected, None) => {
                    !matches!(selected, OperationFinalizationCause::Suspended(_))
                }
                _ => false,
            };
            let unique_settlements = expected_settlements.iter().collect::<HashSet<_>>().len()
                == expected_settlements.len();
            let consumes_terminalize_control = matches!(
                (&operation.pending_control, selected_cause),
                (
                    Some(PendingControlIntent::Terminalize {
                        operation_id: control_operation,
                        cause: control_cause,
                    }),
                    OperationFinalizationCause::GenerationStop(
                        GenerationStopReason::Cancelled {
                            cause: selected_cause,
                        },
                    ),
                ) if control_operation == operation_id && control_cause == selected_cause
            ) || matches!(
                (&operation.pending_control, selected_cause),
                (
                    Some(PendingControlIntent::Terminalize {
                        operation_id: control_operation,
                        cause: control_cause,
                    }),
                    OperationFinalizationCause::Suspended(
                        SuspendedFinalizationCause::Terminalization(selected_cause),
                    ),
                ) if control_operation == operation_id && control_cause == selected_cause
            );
            if !phase_allowed
                || !suspended_matches
                || !unique_settlements
                || operation.finalization.is_some()
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "operation cannot enter finalization",
                ));
            }
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: finalize_intent_id.clone(),
            };
            operation.finalization = Some(OperationFinalizationRecord {
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                started_at: FinalizationStartedAtCursor {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal_commit_id: terminal_commit_id.clone(),
                    event_id: envelope.event_id.clone(),
                    cursor: batch.cursor_after.clone(),
                    commit_class: batch.commit_class.clone(),
                    batch_digest: batch.batch_digest.clone(),
                },
                selected_cause: selected_cause.clone(),
                suspended_cause: suspended_cause.clone(),
                expected_settlements: expected_settlements.clone(),
                settled: Vec::new(),
            });
            if consumes_terminalize_control {
                operation.pending_control = None;
            }
            Ok(())
        }
        OperationPatch::FinalizationSettlementRecorded {
            operation_id,
            finalize_intent_id,
            receipt,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let Some(finalization) = operation.finalization.as_mut() else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "operation finalization does not exist",
                ));
            };
            if finalization.finalize_intent_id != *finalize_intent_id
                || !finalization
                    .expected_settlements
                    .contains(&receipt.settlement_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "settlement does not belong to this finalization",
                ));
            }
            if let Some(applied) = finalization
                .settled
                .iter()
                .find(|applied| applied.settlement_id == receipt.settlement_id)
            {
                if applied == receipt {
                    return Ok(());
                }
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "settlement identity has a different receipt",
                ));
            }
            finalization.settled.push(receipt.clone());
            finalization.settled.sort_by_key(|settled| {
                finalization
                    .expected_settlements
                    .iter()
                    .position(|expected| expected == &settled.settlement_id)
                    .unwrap_or(usize::MAX)
            });
            Ok(())
        }
        OperationPatch::FinalizationDegraded {
            operation_id,
            finalize_intent_id,
            cause,
            last_error: _,
        } => {
            ensure_background_owner(snapshot, envelope, operation_id)?;
            let operation = operation_record_mut(snapshot, envelope, operation_id)?;
            let Some(finalization) = operation.finalization.as_ref() else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "operation finalization does not exist",
                ));
            };
            let cause_matches = match cause {
                FinalizationDegradedCause::MissingFinalization {
                    terminal_commit_id,
                    missing_settlements,
                    ..
                } => {
                    let missing = finalization
                        .expected_settlements
                        .iter()
                        .filter(|expected| {
                            !finalization
                                .settled
                                .iter()
                                .any(|settled| &settled.settlement_id == *expected)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    *terminal_commit_id == finalization.terminal_commit_id
                        && missing_settlements.as_slice() == missing
                }
                FinalizationDegradedCause::TerminalProjectionPending {
                    terminal_commit_id, ..
                } => *terminal_commit_id == finalization.terminal_commit_id,
            };
            if !matches!(operation.phase, OperationPhase::Finalizing { .. })
                || finalization.finalize_intent_id != *finalize_intent_id
                || !cause_matches
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "finalization degradation proof does not match",
                ));
            }
            let proof = AppliedFinalizationDegradedProof {
                finalize_intent_id: finalize_intent_id.clone(),
                selected_cause: finalization.selected_cause.clone(),
                cause: cause.clone(),
            };
            operation.phase = OperationPhase::FinalizingDegraded {
                finalize_intent_id: finalize_intent_id.clone(),
            };
            if let Some(background) = snapshot
                .background_operations
                .iter_mut()
                .find(|background| background.operation_id == *operation_id)
            {
                background.finalizing_degraded = true;
            }
            degraded_finalizations.insert(operation_id.clone(), proof);
            Ok(())
        }
        OperationPatch::Terminal { record } => {
            let queued_position = operation_position(snapshot, &record.operation_id);
            let in_foreground = snapshot
                .foreground_operation
                .as_ref()
                .is_some_and(|operation| operation.operation_id == record.operation_id);
            let background_position = snapshot
                .background_operations
                .iter()
                .position(|operation| operation.operation_id == record.operation_id);
            if let Some(position) = background_position {
                if !matches!(
                    &envelope.scope,
                    SurfaceScope::Background { fence }
                        if fence == &snapshot.background_operations[position].fence
                ) {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::ScopeMismatch,
                        "background terminal owner fence is stale",
                    ));
                }
            }
            let background_history_position = background_position.and_then(|_| {
                snapshot.operation_history.iter().position(|operation| {
                    operation.operation_id == record.operation_id && operation.terminal.is_none()
                })
            });
            let operation = if let Some(position) = queued_position {
                &snapshot.queued_operations[position]
            } else if in_foreground {
                snapshot.foreground_operation.as_ref().unwrap()
            } else if let Some(position) = background_history_position {
                &snapshot.operation_history[position]
            } else {
                if snapshot
                    .operation_history
                    .iter()
                    .any(|operation| operation.operation_id == record.operation_id)
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "terminal operation cannot transition again",
                    ));
                }
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "operation does not exist",
                ));
            };
            let Some(finalization) = operation.finalization.as_ref() else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "operation terminal lacks finalization",
                ));
            };
            let expected_tool_receipts = snapshot
                .tools
                .iter()
                .filter(|tool| {
                    operation
                        .generations
                        .iter()
                        .any(|generation| generation.logical_turn_id == tool.request.turn_id)
                })
                .filter_map(|tool| tool.result.as_ref())
                .map(super::projection::SurfaceToolCompletionReceipt::from_result)
                .collect::<Vec<_>>();
            if record.completion_proof.tool_receipts != expected_tool_receipts {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "terminal completion proof tool receipts do not match recorded tool results",
                ));
            }
            record.completion_proof.validate().map_err(|error| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    format!("terminal completion proof is invalid: {error}"),
                )
            })?;
            if matches!(
                record.completion_proof.verification,
                SurfaceCompletionVerification::Failed { .. }
            ) && matches!(record.terminal, OperationTerminal::Succeeded { .. })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "successful terminal conflicts with failed verifier evidence",
                ));
            }
            let settled_matches = finalization.expected_settlements.len()
                == finalization.settled.len()
                && finalization
                    .expected_settlements
                    .iter()
                    .zip(finalization.settled.iter())
                    .all(|(expected, settled)| expected == &settled.settlement_id);
            let terminal_mapping_matches =
                operation_finalization_matches_terminal(snapshot, operation, finalization, record);
            let terminal_usage_matches = match &record.terminal {
                OperationTerminal::Succeeded { usage } => usage == &record.usage,
                _ => true,
            } && snapshot.usage.active_operation.as_ref().is_none_or(
                |(operation_id, usage)| {
                    operation_id != &record.operation_id || usage == &record.usage
                },
            );
            let terminal_commit_id = commit_id(&batch.commit_class);
            let phase_proof_matches = match &operation.phase {
                OperationPhase::Finalizing { finalize_intent_id } => {
                    finalize_intent_id == &finalization.finalize_intent_id
                        && degraded_finalizations.get(&record.operation_id).is_none()
                }
                OperationPhase::FinalizingDegraded { finalize_intent_id } => degraded_finalizations
                    .get(&record.operation_id)
                    .is_some_and(|proof| {
                        proof.finalize_intent_id == *finalize_intent_id
                            && proof.selected_cause == finalization.selected_cause
                            && match &proof.cause {
                                FinalizationDegradedCause::MissingFinalization {
                                    terminal_commit_id: proved_commit_id,
                                    ..
                                } => {
                                    proved_commit_id == &finalization.terminal_commit_id
                                        && terminal_commit_id == proved_commit_id
                                }
                                FinalizationDegradedCause::TerminalProjectionPending {
                                    terminal_commit_id: proved_commit_id,
                                    terminal_event_id,
                                    ..
                                } => {
                                    proved_commit_id == &finalization.terminal_commit_id
                                        && terminal_commit_id == proved_commit_id
                                        && &envelope.event_id == terminal_event_id
                                }
                            }
                    }),
                OperationPhase::Requested
                | OperationPhase::Admitted
                | OperationPhase::Suspended { .. }
                | OperationPhase::Terminal => false,
            };
            if !phase_proof_matches
                || terminal_commit_id != &finalization.terminal_commit_id
                || record.finalize_intent_id != finalization.finalize_intent_id
                || record.settlement_receipts != finalization.settled
                || !settled_matches
                || !terminal_mapping_matches
                || !terminal_usage_matches
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "terminal does not match finalization",
                ));
            }
            if let Some(position) = background_history_position {
                let operation = &mut snapshot.operation_history[position];
                operation.phase = OperationPhase::Terminal;
                operation.terminal = Some(record.clone());
                snapshot
                    .background_operations
                    .remove(background_position.unwrap());
            } else {
                let mut operation = if let Some(position) = queued_position {
                    snapshot.queued_operations.remove(position)
                } else {
                    snapshot.foreground_operation.take().unwrap()
                };
                operation.phase = OperationPhase::Terminal;
                operation.terminal = Some(record.clone());
                snapshot.operation_history.push(operation);
            }
            degraded_finalizations.remove(&record.operation_id);
            Ok(())
        }
    }
}

fn apply_goal_patch(
    state: &mut SurfaceReducerState,
    envelope: &SurfaceEventEnvelope,
    goal_envelope: &GoalPatchEnvelope,
) -> Result<(), SurfaceReducerError> {
    match &goal_envelope.patch {
        GoalPatch::Created { goal } => {
            if state.snapshot.goal.is_some() {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal already exists",
                ));
            }
            let receipt = &goal_envelope.receipt;
            if goal.thread_id != state.snapshot.thread.thread_id
                || goal.goal_revision.get() != 1
                || receipt.goal_id != goal.goal_id
                || receipt.goal_revision != goal.goal_revision
                || receipt.objective_revision != goal.objective_revision
                || receipt.catalog_revision != goal.catalog_revision
                || receipt.goal_owner_epoch != goal.goal_owner_epoch
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "created goal identity or receipt revision is stale",
                ));
            }
            if goal.token_budget.is_some_and(|budget| budget <= 0)
                || !goal_usage_is_nonnegative(&goal.usage)
                || goal.current_run.as_ref().is_some_and(|run| {
                    !matches!(goal.state, SurfaceGoalState::Active)
                        || !matches!(run.phase, SurfaceGoalRunPhase::Preparing)
                })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "created goal has an invalid initial state",
                ));
            }
            if receipt.row_state
                != (SurfaceGoalReceiptState::Present {
                    state: goal.state.clone(),
                    current_run: goal.current_run.clone(),
                })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "created goal does not match the receipt post-state",
                ));
            }
            let mut goal = goal.clone();
            goal.receipt_digest = receipt.receipt_digest.clone();
            state.snapshot.goal = Some(goal);
            return Ok(());
        }
        GoalPatch::Edited {
            goal_id,
            previous_revision,
            goal,
        } => {
            let current = state.snapshot.goal.as_ref().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            let receipt = &goal_envelope.receipt;
            let expected_objective_revision = current
                .objective_revision
                .get()
                .checked_add(u32::from(goal.objective != current.objective));
            if current.goal_id != *goal_id
                || current.goal_revision != *previous_revision
                || goal.goal_id != *goal_id
                || goal.thread_id != current.thread_id
                || goal.thread_id != state.snapshot.thread.thread_id
                || previous_revision.get().checked_add(1) != Some(goal.goal_revision.get())
                || expected_objective_revision != Some(goal.objective_revision.get())
                || goal.goal_owner_epoch != current.goal_owner_epoch
                || goal.catalog_revision != current.catalog_revision
                || receipt.goal_id != *goal_id
                || receipt.goal_revision != goal.goal_revision
                || receipt.objective_revision != goal.objective_revision
                || receipt.catalog_revision != goal.catalog_revision
                || receipt.goal_owner_epoch != goal.goal_owner_epoch
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "edited goal identity or receipt revision is stale",
                ));
            }
            let current_is_quiescent = current
                .current_run
                .as_ref()
                .is_none_or(|run| matches!(run.phase, SurfaceGoalRunPhase::Settled { .. }));
            if !current_is_quiescent
                || !matches!(goal.state, SurfaceGoalState::Active)
                || goal.current_run != current.current_run
                || goal.usage != current.usage
                || goal.token_budget.is_some_and(|budget| budget <= 0)
                || !goal_usage_is_nonnegative(&goal.usage)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "edited goal violates the no-live-run guard",
                ));
            }
            if receipt.row_state
                != (SurfaceGoalReceiptState::Present {
                    state: goal.state.clone(),
                    current_run: goal.current_run.clone(),
                })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "edited goal does not match the receipt post-state",
                ));
            }
            let mut goal = goal.clone();
            goal.receipt_digest = receipt.receipt_digest.clone();
            state.snapshot.goal = Some(goal);
            return Ok(());
        }
        GoalPatch::Removed {
            goal_id,
            previous_revision,
            tombstone_revision,
        } => {
            let current = state.snapshot.goal.as_ref().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            let receipt = &goal_envelope.receipt;
            if current.goal_id != *goal_id
                || current.goal_revision != *previous_revision
                || previous_revision.get().checked_add(1) != Some(tombstone_revision.get())
                || receipt.goal_id != *goal_id
                || receipt.goal_revision != *tombstone_revision
                || receipt.objective_revision != current.objective_revision
                || receipt.catalog_revision <= current.catalog_revision
                || receipt.goal_owner_epoch != current.goal_owner_epoch
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "removed goal identity or tombstone revision is stale",
                ));
            }
            if receipt.row_state
                != (SurfaceGoalReceiptState::Removed {
                    tombstone_revision: *tombstone_revision,
                })
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "removed goal does not match the tombstone receipt",
                ));
            }
            state.snapshot.goal = None;
            return Ok(());
        }
        GoalPatch::RunStarted { goal_id, goal_run } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "goal run receipt is stale",
                ));
            }
            if matches!(current.state, SurfaceGoalState::Complete { .. })
                || current
                    .current_run
                    .as_ref()
                    .is_some_and(|run| !matches!(run.phase, SurfaceGoalRunPhase::Settled { .. }))
                || !matches!(goal_run.phase, SurfaceGoalRunPhase::Preparing)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal run cannot start from the current state",
                ));
            }
            let expected_row = SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Active,
                current_run: Some(goal_run.clone()),
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal run receipt does not contain the preparing run",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::OuterTurnStarted { identity } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, &identity.goal_id, &goal_envelope.receipt)
                || !goal_identity_matches_owner(&state.snapshot, &current, identity)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "goal outer-turn identity or receipt is stale",
                ));
            }
            let preparing = current.current_run.as_ref().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal run does not exist",
                )
            })?;
            if !matches!(current.state, SurfaceGoalState::Active)
                || preparing.goal_run_id != identity.goal_run_id
                || preparing.operation_id != identity.operation_fence.operation_id
                || !matches!(preparing.phase, SurfaceGoalRunPhase::Preparing)
                || identity.operation_fence.generation_id.get() != 0
                || identity.predecessor_fence.is_some()
                || identity.outer_turn_count != 1
                || identity.attempt != GenerationAttempt::Initial
                || !goal_run_origin_starts_outer_turn(
                    preparing.run_origin,
                    identity.outer_turn_origin,
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal outer turn does not match the preparing run",
                ));
            }
            let mut in_flight = preparing.clone();
            in_flight.phase = SurfaceGoalRunPhase::InFlight {
                outer_turn: goal_outer_turn_receipt(identity),
            };
            let expected_row = SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Active,
                current_run: Some(in_flight),
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal outer-turn receipt does not contain the in-flight run",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::IntentRequested {
            goal_id,
            outer_turn_id,
            ..
        }
        | GoalPatch::IntentAcknowledged {
            goal_id,
            outer_turn_id,
            ..
        } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "goal intent receipt is stale",
                ));
            }
            let current_outer_turn = current.current_run.as_ref().and_then(|run| {
                if let SurfaceGoalRunPhase::InFlight { outer_turn } = &run.phase {
                    Some(outer_turn)
                } else {
                    None
                }
            });
            if !matches!(current.state, SurfaceGoalState::Active)
                || current_outer_turn
                    .is_none_or(|outer_turn| outer_turn.outer_turn_id != *outer_turn_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal intent does not name the active outer turn",
                ));
            }
            if let GoalPatch::IntentAcknowledged { intent, ack, .. } = &goal_envelope.patch {
                let intent_id = goal_intent_id(intent);
                let ack_matches = match ack {
                    SurfaceGoalIntentAck::DeferredToTurnEnd {
                        intent_id: ack_id,
                        pending_depth,
                    } => ack_id == intent_id && *pending_depth > 0,
                    SurfaceGoalIntentAck::AlreadyPending { intent_id: ack_id } => {
                        ack_id == intent_id
                    }
                    SurfaceGoalIntentAck::Rejected { .. } => true,
                    SurfaceGoalIntentAck::BlockedAgainstInactive { .. } => false,
                };
                if !ack_matches {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "goal intent acknowledgement does not match the intent",
                    ));
                }
            }
            let expected_row = SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Active,
                current_run: current.current_run.clone(),
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal intent receipt does not preserve the in-flight run",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::OuterTurnFinished {
            identity, usage, ..
        } => {
            let mut current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, &identity.goal_id, &goal_envelope.receipt)
                || !goal_identity_matches_owner(&state.snapshot, &current, identity)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "finished goal outer-turn identity or receipt is stale",
                ));
            }
            let Some(in_flight) = current.current_run.as_ref() else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal run does not exist",
                ));
            };
            let state_can_settle = matches!(
                current.state,
                SurfaceGoalState::Active
                    | SurfaceGoalState::Paused {
                        reason: SurfaceGoalPauseReason::User
                            | SurfaceGoalPauseReason::Infrastructure,
                        ..
                    }
            );
            if !state_can_settle
                || !goal_run_is_in_flight_for_identity(in_flight, identity)
                || !goal_usage_is_nonnegative(usage)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal outer turn cannot finish from the current state",
                ));
            }
            let mut settled = in_flight.clone();
            settled.phase = SurfaceGoalRunPhase::Settled {
                last_outer_turn: Some(goal_outer_turn_receipt(identity)),
            };
            let expected_row = SurfaceGoalReceiptState::Present {
                state: current.state.clone(),
                current_run: Some(settled),
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "finished goal outer-turn receipt does not contain the settled run",
                ));
            }
            current.usage = accumulate_goal_usage(&current.usage, usage);
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::VerificationCompleted { identity, .. } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, &identity.goal_id, &goal_envelope.receipt)
                || !goal_identity_matches_owner(&state.snapshot, &current, identity)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "goal verification identity or receipt is stale",
                ));
            }
            let settled_matches = current
                .current_run
                .as_ref()
                .is_some_and(|run| goal_run_is_settled_for_identity(run, identity));
            if !settled_matches {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal verification does not name the settled outer turn",
                ));
            }
            let expected_row = SurfaceGoalReceiptState::Present {
                state: current.state.clone(),
                current_run: current.current_run.clone(),
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal verification receipt does not preserve the settled run",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::Transitioned {
            goal_id,
            transition,
        } => {
            let mut current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "goal transition receipt is stale",
                ));
            }
            if transition.previous != current.state
                || matches!(current.state, SurfaceGoalState::Complete { .. })
                || transition.previous == transition.next
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal state transition is invalid",
                ));
            }
            let SurfaceGoalReceiptState::Present {
                state: receipt_state,
                current_run: receipt_run,
            } = &goal_envelope.receipt.row_state
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal transition receipt cannot remove the goal",
                ));
            };
            let run_matches = match &transition.next {
                SurfaceGoalState::Active => receipt_run == &current.current_run,
                SurfaceGoalState::Paused { .. } => {
                    receipt_run.is_none()
                        || receipt_run.as_ref().is_some_and(|run| {
                            current.current_run.as_ref() == Some(run)
                                && matches!(
                                    run.phase,
                                    SurfaceGoalRunPhase::Preparing
                                        | SurfaceGoalRunPhase::InFlight { .. }
                                )
                        })
                }
                SurfaceGoalState::Blocked { .. }
                | SurfaceGoalState::BudgetLimited
                | SurfaceGoalState::Complete { .. } => receipt_run.is_none(),
            };
            if receipt_state != &transition.next || !run_matches {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal transition receipt does not match the selected post-state",
                ));
            }
            current.last_transition = Some(transition.clone());
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::Paused {
            goal_id,
            goal_run_id,
            outer_turn_id,
            state: paused_state,
        } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "paused goal receipt is stale",
                ));
            }
            if !matches!(paused_state, SurfaceGoalState::Paused { .. })
                || matches!(current.state, SurfaceGoalState::Complete { .. })
                || !goal_optional_run_identity_matches(
                    current.current_run.as_ref(),
                    goal_run_id.as_ref(),
                    outer_turn_id.as_ref(),
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "paused goal state or run identity is invalid",
                ));
            }
            let SurfaceGoalReceiptState::Present {
                state: receipt_state,
                current_run: receipt_run,
            } = &goal_envelope.receipt.row_state
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "paused goal receipt cannot remove the goal",
                ));
            };
            let retained_run_is_valid = receipt_run.is_none()
                || receipt_run.as_ref().is_some_and(|run| {
                    current.current_run.as_ref() == Some(run)
                        && matches!(
                            run.phase,
                            SurfaceGoalRunPhase::Preparing | SurfaceGoalRunPhase::InFlight { .. }
                        )
                });
            if receipt_state != paused_state || !retained_run_is_valid {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "paused goal receipt does not match the inactive post-state",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::Recovered {
            goal_id,
            stale_run,
            recovery_message,
            discarded_continuation,
        } => {
            let current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "recovered goal receipt is stale",
                ));
            }
            if !discarded_continuation.get()
                || stale_run.close_reason != SurfaceGoalCloseReason::Recovery
                || stale_run.store_commit_id != goal_envelope.receipt.store_commit_id
                || current.current_run.as_ref() != Some(&stale_run.run)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal recovery does not close the exact stale run",
                ));
            }
            if state.goal_recoveries.iter().any(|record| {
                record.goal_id == *goal_id
                    && record.stale_run.run == stale_run.run
                    && (record.stale_run.store_commit_id != stale_run.store_commit_id
                        || record.stale_run.receipt_digest != stale_run.receipt_digest
                        || record.goal_receipt_digest != goal_envelope.receipt.receipt_digest)
            }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "stale goal run already has a different close receipt",
                ));
            }
            let expected_row = SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Paused {
                    reason: SurfaceGoalPauseReason::Recovery,
                    message: recovery_message.clone(),
                },
                current_run: None,
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal recovery receipt does not contain the recovery pause state",
                ));
            }
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            state.goal_recoveries.push(AppliedGoalRecoveryRecord {
                goal_id: goal_id.clone(),
                stale_run: stale_run.clone(),
                goal_receipt_digest: goal_envelope.receipt.receipt_digest.clone(),
            });
            return Ok(());
        }
        GoalPatch::Completed {
            goal_id,
            goal_run_id,
            evidence,
            usage,
        } => {
            let mut current = state.snapshot.goal.as_ref().cloned().ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "goal does not exist",
                )
            })?;
            if !goal_receipt_is_contiguous(&current, goal_id, &goal_envelope.receipt) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "completed goal receipt is stale",
                ));
            }
            let current_run_id = current.current_run.as_ref().map(|run| &run.goal_run_id);
            if matches!(current.state, SurfaceGoalState::Complete { .. })
                || current_run_id != goal_run_id.as_ref()
                || !goal_usage_is_nonnegative(usage)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "goal completion state, run identity, or usage is invalid",
                ));
            }
            let expected_row = SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Complete {
                    evidence: evidence.clone(),
                },
                current_run: None,
            };
            if goal_envelope.receipt.row_state != expected_row {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::GoalReceiptMismatch,
                    "goal completion receipt does not match evidence or closed-run state",
                ));
            }
            current.usage = usage.clone();
            state.snapshot.goal = Some(goal_with_present_receipt(current, &goal_envelope.receipt));
            return Ok(());
        }
        GoalPatch::ContinuationDecided { .. } => {}
    }

    let GoalPatch::ContinuationDecided {
        goal_id,
        predecessor,
        decision,
    } = &goal_envelope.patch
    else {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::IllegalTransition,
            "goal patch is not reducible yet",
        ));
    };
    let successor_authorization = match decision {
        GoalContinuationDecision::Admitted { successor, .. } => Some(successor.clone()),
        GoalContinuationDecision::Stopped { .. } => None,
    };
    let goal = state.snapshot.goal.as_mut().ok_or_else(|| {
        event_error(
            envelope,
            SurfaceReducerErrorCode::MissingIdentity,
            "goal does not exist",
        )
    })?;
    if goal.goal_id != *goal_id
        || predecessor.goal_id != *goal_id
        || predecessor.operation_fence.thread_id != state.snapshot.thread.thread_id
        || predecessor.operation_fence.thread_owner_epoch != state.snapshot.thread.owner_epoch
        || goal_envelope.receipt.goal_id != *goal_id
        || goal.goal_revision.get().checked_add(1)
            != Some(goal_envelope.receipt.goal_revision.get())
        || goal_envelope.receipt.objective_revision != goal.objective_revision
        || goal_envelope.receipt.catalog_revision != goal.catalog_revision
        || goal_envelope.receipt.goal_owner_epoch != goal.goal_owner_epoch
    {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::StaleRevision,
            "goal continuation identity or revision is stale",
        ));
    }
    if successor_authorization.is_some()
        && state
            .goal_successor_authorizations
            .iter()
            .any(|authorization| authorization.predecessor == predecessor.operation_fence)
    {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::DuplicateTransition,
            "goal predecessor already authorized a successor",
        ));
    }
    let valid_post_state = match decision {
        GoalContinuationDecision::Admitted { successor, .. } => {
            let expected_origin = match successor.outer_turn_origin {
                GoalOuterTurnOrigin::User => SurfaceGoalOuterTurnReceiptOrigin::User,
                GoalOuterTurnOrigin::Resume => SurfaceGoalOuterTurnReceiptOrigin::Resume,
                GoalOuterTurnOrigin::Continuation => {
                    SurfaceGoalOuterTurnReceiptOrigin::Continuation
                }
                GoalOuterTurnOrigin::WorkflowNotification => {
                    SurfaceGoalOuterTurnReceiptOrigin::WorkflowNotification
                }
            };
            let receipt_run = match &goal_envelope.receipt.row_state {
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(run),
                } => Some(run),
                _ => None,
            };
            successor.goal_id == *goal_id
                && successor.goal_run_id == predecessor.goal_run_id
                && successor.operation_fence.operation_id
                    == predecessor.operation_fence.operation_id
                && predecessor
                    .operation_fence
                    .generation_id
                    .get()
                    .checked_add(1)
                    == Some(successor.operation_fence.generation_id.get())
                && successor.predecessor_fence.as_ref() == Some(&predecessor.operation_fence)
                && predecessor.outer_turn_count.checked_add(1) == Some(successor.outer_turn_count)
                && successor.objective_revision == goal_envelope.receipt.objective_revision
                && receipt_run.is_some_and(|run| {
                    run.goal_run_id == successor.goal_run_id
                        && run.operation_id == successor.operation_fence.operation_id
                        && matches!(
                            &run.phase,
                            SurfaceGoalRunPhase::InFlight { outer_turn }
                                if outer_turn.outer_turn_id == successor.goal_outer_turn_id
                                    && outer_turn.origin == expected_origin
                                    && outer_turn.outer_turn_count == successor.outer_turn_count
                        )
                })
        }
        GoalContinuationDecision::Stopped {
            goal_state,
            outer_turn_count,
            ..
        } => {
            *outer_turn_count == predecessor.outer_turn_count
                && matches!(
                    &goal_envelope.receipt.row_state,
                    SurfaceGoalReceiptState::Present { state, .. } if state == goal_state
                )
        }
    };
    if !valid_post_state {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::GoalReceiptMismatch,
            "goal receipt does not match the continuation decision",
        ));
    }
    let SurfaceGoalReceiptState::Present {
        state: next_state,
        current_run,
    } = &goal_envelope.receipt.row_state
    else {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::GoalReceiptMismatch,
            "goal continuation receipt cannot remove the goal",
        ));
    };
    goal.goal_revision = goal_envelope.receipt.goal_revision;
    goal.objective_revision = goal_envelope.receipt.objective_revision;
    goal.catalog_revision = goal_envelope.receipt.catalog_revision;
    goal.goal_owner_epoch = goal_envelope.receipt.goal_owner_epoch;
    goal.receipt_digest = goal_envelope.receipt.receipt_digest;
    goal.state = next_state.clone();
    goal.current_run = current_run.clone();
    if successor_authorization.is_some() {
        state
            .goal_successor_authorizations
            .push(AppliedGoalSuccessorAuthorization {
                predecessor: predecessor.operation_fence.clone(),
            });
    }
    Ok(())
}

fn goal_usage_is_nonnegative(usage: &GoalUsage) -> bool {
    usage.charged_input_tokens >= 0
        && usage.output_tokens >= 0
        && usage.cache_tokens >= 0
        && usage.verifier_tokens >= 0
        && usage.cost_micros >= 0
        && usage.elapsed_seconds >= 0
}

fn accumulate_goal_usage(total: &GoalUsage, delta: &GoalUsage) -> GoalUsage {
    GoalUsage {
        charged_input_tokens: total
            .charged_input_tokens
            .saturating_add(delta.charged_input_tokens),
        output_tokens: total.output_tokens.saturating_add(delta.output_tokens),
        cache_tokens: total.cache_tokens.saturating_add(delta.cache_tokens),
        verifier_tokens: total.verifier_tokens.saturating_add(delta.verifier_tokens),
        cost_micros: total.cost_micros.saturating_add(delta.cost_micros),
        elapsed_seconds: total.elapsed_seconds.saturating_add(delta.elapsed_seconds),
    }
}

fn goal_receipt_is_contiguous(
    goal: &SurfaceGoal,
    goal_id: &SurfaceGoalId,
    receipt: &SurfaceGoalStoreReceipt,
) -> bool {
    goal.goal_id == *goal_id
        && receipt.goal_id == *goal_id
        && goal.goal_revision.get().checked_add(1) == Some(receipt.goal_revision.get())
        && receipt.objective_revision == goal.objective_revision
        && receipt.catalog_revision == goal.catalog_revision
        && receipt.goal_owner_epoch == goal.goal_owner_epoch
}

fn goal_with_present_receipt(
    mut goal: SurfaceGoal,
    receipt: &SurfaceGoalStoreReceipt,
) -> SurfaceGoal {
    let SurfaceGoalReceiptState::Present { state, current_run } = &receipt.row_state else {
        unreachable!("caller validates a present goal receipt")
    };
    goal.goal_revision = receipt.goal_revision;
    goal.objective_revision = receipt.objective_revision;
    goal.catalog_revision = receipt.catalog_revision;
    goal.goal_owner_epoch = receipt.goal_owner_epoch;
    goal.receipt_digest = receipt.receipt_digest.clone();
    goal.state = state.clone();
    goal.current_run = current_run.clone();
    goal
}

fn goal_identity_matches_owner(
    snapshot: &SurfaceSnapshot,
    goal: &SurfaceGoal,
    identity: &SurfaceGoalGenerationIdentity,
) -> bool {
    identity.goal_id == goal.goal_id
        && identity.objective_revision == goal.objective_revision
        && identity.operation_fence.thread_id == snapshot.thread.thread_id
        && identity.operation_fence.thread_owner_epoch == snapshot.thread.owner_epoch
}

fn goal_outer_turn_origin(origin: GoalOuterTurnOrigin) -> SurfaceGoalOuterTurnReceiptOrigin {
    match origin {
        GoalOuterTurnOrigin::User => SurfaceGoalOuterTurnReceiptOrigin::User,
        GoalOuterTurnOrigin::Resume => SurfaceGoalOuterTurnReceiptOrigin::Resume,
        GoalOuterTurnOrigin::Continuation => SurfaceGoalOuterTurnReceiptOrigin::Continuation,
        GoalOuterTurnOrigin::WorkflowNotification => {
            SurfaceGoalOuterTurnReceiptOrigin::WorkflowNotification
        }
    }
}

fn goal_outer_turn_receipt(
    identity: &SurfaceGoalGenerationIdentity,
) -> SurfaceGoalOuterTurnReceipt {
    SurfaceGoalOuterTurnReceipt {
        outer_turn_id: identity.goal_outer_turn_id.clone(),
        origin: goal_outer_turn_origin(identity.outer_turn_origin),
        outer_turn_count: identity.outer_turn_count,
    }
}

fn goal_run_origin_starts_outer_turn(
    run_origin: SurfaceGoalRunOrigin,
    outer_turn_origin: GoalOuterTurnOrigin,
) -> bool {
    matches!(
        (run_origin, outer_turn_origin),
        (SurfaceGoalRunOrigin::User, GoalOuterTurnOrigin::User)
            | (SurfaceGoalRunOrigin::Resume, GoalOuterTurnOrigin::Resume)
            | (
                SurfaceGoalRunOrigin::WorkflowNotification,
                GoalOuterTurnOrigin::WorkflowNotification
            )
    )
}

fn goal_run_is_in_flight_for_identity(
    run: &SurfaceGoalRun,
    identity: &SurfaceGoalGenerationIdentity,
) -> bool {
    run.goal_run_id == identity.goal_run_id
        && run.operation_id == identity.operation_fence.operation_id
        && matches!(
            &run.phase,
            SurfaceGoalRunPhase::InFlight { outer_turn }
                if *outer_turn == goal_outer_turn_receipt(identity)
        )
}

fn goal_run_is_settled_for_identity(
    run: &SurfaceGoalRun,
    identity: &SurfaceGoalGenerationIdentity,
) -> bool {
    run.goal_run_id == identity.goal_run_id
        && run.operation_id == identity.operation_fence.operation_id
        && matches!(
            &run.phase,
            SurfaceGoalRunPhase::Settled {
                last_outer_turn: Some(outer_turn),
            } if *outer_turn == goal_outer_turn_receipt(identity)
        )
}

fn goal_intent_id(intent: &SurfaceGoalIntent) -> &SurfaceGoalIntentId {
    match intent {
        SurfaceGoalIntent::Complete { intent_id, .. }
        | SurfaceGoalIntent::Blocked { intent_id, .. } => intent_id,
    }
}

fn goal_optional_run_identity_matches(
    run: Option<&SurfaceGoalRun>,
    goal_run_id: Option<&SurfaceGoalRunId>,
    outer_turn_id: Option<&SurfaceGoalOuterTurnId>,
) -> bool {
    match run {
        None => goal_run_id.is_none() && outer_turn_id.is_none(),
        Some(run) => {
            let expected_outer_turn_id = match &run.phase {
                SurfaceGoalRunPhase::Preparing => None,
                SurfaceGoalRunPhase::InFlight { outer_turn } => Some(&outer_turn.outer_turn_id),
                SurfaceGoalRunPhase::Settled { last_outer_turn } => last_outer_turn
                    .as_ref()
                    .map(|outer_turn| &outer_turn.outer_turn_id),
            };
            goal_run_id == Some(&run.goal_run_id) && outer_turn_id == expected_outer_turn_id
        }
    }
}

fn workflow_status_transition_allowed(
    from: SurfaceWorkflowStatus,
    to: SurfaceWorkflowStatus,
) -> bool {
    use SurfaceWorkflowStatus::*;
    matches!(
        (from, to),
        (Queued, Running | Failed | Cancelled)
            | (
                Running,
                Paused | Stopping | Completed | Failed | Cancelled | AsyncLaunched
            )
            | (Paused, Running | Stopping | Failed | Cancelled)
            | (Stopping, Stopped | Failed | Cancelled)
            | (
                AsyncLaunched,
                Running | Stopping | Completed | Failed | Cancelled
            )
    )
}

fn workflow_revision_is_contiguous(expected: WorkflowRevision, next: WorkflowRevision) -> bool {
    expected
        .get()
        .checked_add(1)
        .is_some_and(|expected_next| next.get() == expected_next)
}

fn workflow_agent_attempt_identity_matches(
    current: &SurfaceWorkflowAgent,
    next: &SurfaceWorkflowAgent,
) -> bool {
    current.agent_id == next.agent_id
        && current.attempt == next.attempt
        && current.phase == next.phase
}

fn workflow_for_fence_mut<'a>(
    snapshot: &'a mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    fence: &SurfaceWorkflowFence,
    next_revision: WorkflowRevision,
) -> Result<&'a mut SurfaceWorkflow, SurfaceReducerError> {
    let Some(workflow) = snapshot
        .workflows
        .iter_mut()
        .find(|workflow| workflow.workflow_run_id == fence.workflow_run_id)
    else {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::MissingIdentity,
            "workflow does not exist",
        ));
    };
    if workflow.revision != fence.workflow_revision
        || workflow.parent != fence.parent
        || !workflow_revision_is_contiguous(fence.workflow_revision, next_revision)
    {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::StaleRevision,
            "workflow fence or revision is stale",
        ));
    }
    Ok(workflow)
}

fn apply_workflow_status(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    fence: &SurfaceWorkflowFence,
    next_revision: WorkflowRevision,
    target: SurfaceWorkflowStatus,
    error: Option<DisplayText>,
) -> Result<(), SurfaceReducerError> {
    let workflow = workflow_for_fence_mut(snapshot, envelope, fence, next_revision)?;
    if !workflow_status_transition_allowed(workflow.status, target) {
        return Err(event_error(
            envelope,
            SurfaceReducerErrorCode::IllegalTransition,
            "workflow status transition is not allowed",
        ));
    }
    workflow.revision = next_revision;
    workflow.status = target;
    workflow.error = error;
    Ok(())
}

fn apply_workflow_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &WorkflowPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        WorkflowPatch::Started { workflow } => {
            if snapshot
                .workflows
                .iter()
                .any(|value| value.workflow_run_id == workflow.workflow_run_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "workflow already exists",
                ));
            }
            if workflow.revision.get() != 1
                || !matches!(
                    workflow.status,
                    SurfaceWorkflowStatus::Queued | SurfaceWorkflowStatus::Running
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow creation transition is not allowed",
                ));
            }
            snapshot.workflows.push(workflow.clone());
            Ok(())
        }
        WorkflowPatch::Resumed {
            fence,
            next_revision,
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Running,
            None,
        ),
        WorkflowPatch::Paused {
            fence,
            next_revision,
            ..
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Paused,
            None,
        ),
        WorkflowPatch::Stopping {
            fence,
            next_revision,
            ..
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Stopping,
            None,
        ),
        WorkflowPatch::Stopped {
            fence,
            next_revision,
            ..
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Stopped,
            None,
        ),
        WorkflowPatch::AsyncLaunched {
            fence,
            next_revision,
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::AsyncLaunched,
            None,
        ),
        WorkflowPatch::Completed {
            fence,
            next_revision,
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Completed,
            None,
        ),
        WorkflowPatch::Failed {
            fence,
            next_revision,
            error,
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Failed,
            Some(error.clone()),
        ),
        WorkflowPatch::Cancelled {
            fence,
            next_revision,
            ..
        } => apply_workflow_status(
            snapshot,
            envelope,
            fence,
            *next_revision,
            SurfaceWorkflowStatus::Cancelled,
            None,
        ),
        WorkflowPatch::PhaseStarted {
            fence,
            next_revision,
            phase,
        } => {
            let workflow = workflow_for_fence_mut(snapshot, envelope, fence, *next_revision)?;
            if phase.status != SurfaceWorkflowStatus::Running
                || workflow.phases.iter().any(|value| value.name == phase.name)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow phase start transition is not allowed",
                ));
            }
            workflow.phases.push(phase.clone());
            workflow.revision = *next_revision;
            Ok(())
        }
        WorkflowPatch::PhaseCompleted {
            fence,
            next_revision,
            phase,
        } => {
            let workflow = workflow_for_fence_mut(snapshot, envelope, fence, *next_revision)?;
            let Some(current) = workflow
                .phases
                .iter_mut()
                .find(|value| value.name == phase.name)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "workflow phase does not exist",
                ));
            };
            if current.status != SurfaceWorkflowStatus::Running
                || !matches!(
                    phase.status,
                    SurfaceWorkflowStatus::Completed
                        | SurfaceWorkflowStatus::Failed
                        | SurfaceWorkflowStatus::Stopped
                        | SurfaceWorkflowStatus::Cancelled
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow phase transition is not allowed",
                ));
            }
            *current = phase.clone();
            workflow.revision = *next_revision;
            Ok(())
        }
        WorkflowPatch::AgentStarted {
            fence,
            next_revision,
            agent,
        }
        | WorkflowPatch::AgentCached {
            fence,
            next_revision,
            agent,
        }
        | WorkflowPatch::AgentCompleted {
            fence,
            next_revision,
            agent,
        }
        | WorkflowPatch::AgentFailed {
            fence,
            next_revision,
            agent,
        }
        | WorkflowPatch::AgentCancelled {
            fence,
            next_revision,
            agent,
        } => {
            let discriminant_matches = matches!(
                (patch, agent.status),
                (
                    WorkflowPatch::AgentStarted { .. },
                    SurfaceWorkflowAgentStatus::Pending | SurfaceWorkflowAgentStatus::Running
                ) | (
                    WorkflowPatch::AgentCached { .. },
                    SurfaceWorkflowAgentStatus::Cached
                ) | (
                    WorkflowPatch::AgentCompleted { .. },
                    SurfaceWorkflowAgentStatus::Completed
                ) | (
                    WorkflowPatch::AgentFailed { .. },
                    SurfaceWorkflowAgentStatus::Failed
                ) | (
                    WorkflowPatch::AgentCancelled { .. },
                    SurfaceWorkflowAgentStatus::Cancelled
                )
            );
            if !discriminant_matches {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow agent patch does not match its target state",
                ));
            }
            let workflow = workflow_for_fence_mut(snapshot, envelope, fence, *next_revision)?;
            let existing = workflow
                .agents
                .iter_mut()
                .find(|value| value.agent_id == agent.agent_id && value.attempt == agent.attempt);
            if existing
                .as_ref()
                .is_some_and(|current| !workflow_agent_attempt_identity_matches(current, agent))
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow agent transition changed the attempt identity",
                ));
            }
            let allowed = match (existing.as_ref().map(|value| value.status), agent.status) {
                (
                    None,
                    SurfaceWorkflowAgentStatus::Pending | SurfaceWorkflowAgentStatus::Running,
                ) => true,
                (
                    Some(SurfaceWorkflowAgentStatus::Pending),
                    SurfaceWorkflowAgentStatus::Running,
                )
                | (Some(SurfaceWorkflowAgentStatus::Pending), SurfaceWorkflowAgentStatus::Cached)
                | (
                    Some(SurfaceWorkflowAgentStatus::Pending),
                    SurfaceWorkflowAgentStatus::Cancelled,
                )
                | (
                    Some(SurfaceWorkflowAgentStatus::Running),
                    SurfaceWorkflowAgentStatus::Completed,
                )
                | (Some(SurfaceWorkflowAgentStatus::Running), SurfaceWorkflowAgentStatus::Failed)
                | (
                    Some(SurfaceWorkflowAgentStatus::Running),
                    SurfaceWorkflowAgentStatus::Cancelled,
                ) => true,
                _ => false,
            };
            if !allowed {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "workflow agent attempt transition is not allowed",
                ));
            }
            if let Some(existing) = existing {
                *existing = agent.clone();
            } else {
                if workflow
                    .agents
                    .iter()
                    .any(|value| value.agent_id == agent.agent_id && value.attempt >= agent.attempt)
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::InvalidOrdering,
                        "workflow agent retry attempt is not increasing",
                    ));
                }
                workflow.agents.push(agent.clone());
            }
            workflow.revision = *next_revision;
            Ok(())
        }
        WorkflowPatch::ResultReady {
            fence,
            next_revision,
            result,
        } => {
            let workflow = workflow_for_fence_mut(snapshot, envelope, fence, *next_revision)?;
            if workflow.result.is_some() {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "workflow result already exists",
                ));
            }
            workflow.result = Some(result.clone());
            workflow.revision = *next_revision;
            Ok(())
        }
        WorkflowPatch::ResultAcknowledged {
            fence,
            next_revision,
            result_id,
            operation_id,
        } => {
            let workflow = workflow_for_fence_mut(snapshot, envelope, fence, *next_revision)?;
            let Some(result) = workflow.result.as_mut() else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "workflow result does not exist",
                ));
            };
            if result.result_id != *result_id || result.acknowledged_by_operation.is_some() {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "workflow result acknowledgement conflicts",
                ));
            }
            result.acknowledged_by_operation = Some(operation_id.clone());
            workflow.revision = *next_revision;
            Ok(())
        }
    }
}

fn apply_subagent_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &SubagentPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        SubagentPatch::Started { subagent, .. } => {
            let subagent = subagent.as_subagent();
            if snapshot
                .subagents
                .iter()
                .any(|value| value.subagent_id == subagent.subagent_id)
                || subagent.revision.get() != 1
                || subagent.source.source_sequence != 1
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "subagent start transition is not allowed",
                ));
            }
            if !subagent_source_matches_owner(&subagent.owner, &subagent.source) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::ScopeMismatch,
                    "subagent source cursor does not match its owner",
                ));
            }
            if let SurfaceSubagentOwner::DetachedTask { owner } = &subagent.owner {
                let Some(task) = snapshot
                    .tasks
                    .iter()
                    .find(|task| task.task_id == owner.task_id)
                else {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "detached subagent owner task does not exist",
                    ));
                };
                if task.task_type != SurfaceTaskType::Subagent
                    || task.subagent_id.as_ref() != Some(&subagent.subagent_id)
                    || task.revision < owner.task_revision
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::ScopeMismatch,
                        "detached subagent owner task binding is invalid",
                    ));
                }
            }
            snapshot.subagents.push(subagent.clone());
            Ok(())
        }
        SubagentPatch::Progress {
            subagent_id,
            expected_revision,
            next_revision,
            owner,
            source,
            activity,
            turn,
            usage,
        } => {
            let Some(subagent) = snapshot
                .subagents
                .iter_mut()
                .find(|value| value.subagent_id == *subagent_id)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "subagent does not exist",
                ));
            };
            if subagent.revision != *expected_revision
                || !expected_revision
                    .get()
                    .checked_add(1)
                    .is_some_and(|expected_next| next_revision.get() == expected_next)
                || subagent.owner != *owner
                || !subagent_source_is_next(&subagent.source, source)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "subagent fence or revision is stale",
                ));
            }
            if subagent.status != SurfaceSubagentStatus::Running {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "terminal subagent is absorbing",
                ));
            }
            subagent.revision = *next_revision;
            subagent.source = source.clone();
            subagent.activity = Some(activity.clone());
            subagent.turn = *turn;
            subagent.usage = usage.clone();
            Ok(())
        }
        SubagentPatch::Completed {
            subagent_id,
            expected_revision,
            next_revision,
            owner,
            source,
            status,
            output,
            error,
            usage,
        } => {
            let Some(subagent) = snapshot
                .subagents
                .iter_mut()
                .find(|value| value.subagent_id == *subagent_id)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "absent subagent cannot complete",
                ));
            };
            if subagent.revision != *expected_revision
                || !expected_revision
                    .get()
                    .checked_add(1)
                    .is_some_and(|expected_next| next_revision.get() == expected_next)
                || subagent.owner != *owner
                || !subagent_source_is_next(&subagent.source, source)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "subagent fence or revision is stale",
                ));
            }
            if subagent.status != SurfaceSubagentStatus::Running {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "terminal subagent is absorbing",
                ));
            }
            subagent.revision = *next_revision;
            subagent.source = source.clone();
            subagent.status = match status {
                SurfaceSubagentTerminalStatus::Completed => SurfaceSubagentStatus::Completed,
                SurfaceSubagentTerminalStatus::Failed => SurfaceSubagentStatus::Failed,
                SurfaceSubagentTerminalStatus::Cancelled => SurfaceSubagentStatus::Cancelled,
            };
            subagent.output = output.clone();
            subagent.error = error.clone();
            subagent.usage = usage.clone();
            Ok(())
        }
    }
}

fn subagent_source_matches_owner(
    owner: &super::projection::SurfaceSubagentOwner,
    source: &SurfaceSubagentSource,
) -> bool {
    source.source_sequence > 0
        && match owner {
            SurfaceSubagentOwner::Generation { .. } => true,
            SurfaceSubagentOwner::DetachedTask { owner } => source.attempt_id == owner.attempt_id,
        }
}

fn subagent_source_is_next(previous: &SurfaceSubagentSource, next: &SurfaceSubagentSource) -> bool {
    previous.attempt_id == next.attempt_id
        && previous
            .source_sequence
            .checked_add(1)
            .is_some_and(|expected| expected == next.source_sequence)
        && next.source_commit_id != previous.source_commit_id
}

fn task_status_transition_allowed(from: SurfaceTaskStatus, to: SurfaceTaskStatus) -> bool {
    use SurfaceTaskStatus::*;
    matches!(
        (from, to),
        (
            Queued,
            Running | Paused | Stopping | Stopped | Failed | Cancelled
        ) | (
            Running,
            Paused | Stopping | Stopped | Completed | Failed | ApprovalRequired | Cancelled
        ) | (
            ApprovalRequired,
            Running | Stopping | Stopped | Failed | Cancelled
        ) | (Paused, Running | Stopping | Stopped | Failed | Cancelled)
            | (Stopping, Stopped | Failed | Cancelled)
    )
}

fn task_revision_is_contiguous(expected: TaskRevision, next: TaskRevision) -> bool {
    expected.get().checked_add(1) == Some(next.get())
}

fn historical_terminal_task_creation_allowed(task: &SurfaceTask) -> bool {
    task.revision.get() == 1
        && task.task_type == SurfaceTaskType::MainSession
        && matches!(
            task.status,
            SurfaceTaskStatus::Completed
                | SurfaceTaskStatus::Stopped
                | SurfaceTaskStatus::Cancelled
        )
        && task.completed_at.is_some()
        && !task.backgrounded
        && task.parent_operation.is_none()
        && task.parent_task_id.is_none()
        && task.background_fence.is_none()
        && task.workflow_run_id.is_none()
        && task.subagent_id.is_none()
        && task.pending_interaction_id.is_none()
}

fn validate_task_parent_graph(
    tasks: &[SurfaceTask],
    envelope: &SurfaceEventEnvelope,
) -> Result<(), SurfaceReducerError> {
    for task in tasks {
        let mut current = task;
        let mut seen = HashSet::new();
        let mut depth = 0_u8;
        loop {
            if !seen.insert(current.task_id.clone()) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task parent graph contains a cycle",
                ));
            }
            let Some(parent_task_id) = current.parent_task_id.as_ref() else {
                break;
            };
            depth = depth.checked_add(1).ok_or_else(|| {
                event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task parent depth overflow",
                )
            })?;
            if depth > 32 {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task parent depth exceeds 32",
                ));
            }
            current = tasks
                .iter()
                .find(|candidate| candidate.task_id == *parent_task_id)
                .ok_or_else(|| {
                    event_error(
                        envelope,
                        SurfaceReducerErrorCode::MissingIdentity,
                        "task parent does not exist",
                    )
                })?;
        }
    }
    Ok(())
}

fn apply_task_patch(
    snapshot: &mut SurfaceSnapshot,
    envelope: &SurfaceEventEnvelope,
    patch: &TaskPatch,
) -> Result<(), SurfaceReducerError> {
    match patch {
        TaskPatch::Upserted {
            expected_revision,
            task,
        } => {
            if expected_revision.is_some() {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "live upsert with expected revision is not creation",
                ));
            }
            if snapshot
                .tasks
                .iter()
                .any(|value| value.task_id == task.task_id)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "task already exists",
                ));
            }
            let mut candidate_tasks = snapshot.tasks.clone();
            candidate_tasks.push(task.clone());
            validate_task_parent_graph(&candidate_tasks, envelope)?;
            if task.revision.get() != 1
                || !matches!(
                    task.status,
                    SurfaceTaskStatus::Queued | SurfaceTaskStatus::Running
                )
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task creation transition is not allowed",
                ));
            }
            snapshot.tasks.push(task.clone());
            Ok(())
        }
        TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status,
            completed_at,
            result,
            error,
        } => {
            let Some(task) = snapshot
                .tasks
                .iter_mut()
                .find(|task| task.task_id == *task_id)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "task does not exist",
                ));
            };
            if task.revision != *expected_revision
                || !task_revision_is_contiguous(*expected_revision, *next_revision)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "task revision is stale or noncontiguous",
                ));
            }
            if !task_status_transition_allowed(task.status, *status) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task status transition is not allowed",
                ));
            }
            task.revision = *next_revision;
            task.status = *status;
            task.completed_at = *completed_at;
            task.result = result.clone();
            task.error = error.clone();
            Ok(())
        }
        TaskPatch::InteractionChanged {
            task_id,
            expected_revision,
            next_revision,
            status,
            pending_interaction_id,
        } => {
            let Some(task) = snapshot
                .tasks
                .iter_mut()
                .find(|task| task.task_id == *task_id)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "task does not exist",
                ));
            };
            if task.revision != *expected_revision
                || !task_revision_is_contiguous(*expected_revision, *next_revision)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "task interaction revision is stale or noncontiguous",
                ));
            }
            if !task_status_transition_allowed(task.status, *status) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task interaction status transition is not allowed",
                ));
            }
            let valid_request = task.pending_interaction_id.is_none()
                && pending_interaction_id.is_some()
                && *status == SurfaceTaskStatus::ApprovalRequired;
            let valid_resolution = task.pending_interaction_id.is_some()
                && pending_interaction_id.is_none()
                && *status == SurfaceTaskStatus::Running;
            if !valid_request && !valid_resolution {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task interaction transition is not a request or resolution",
                ));
            }
            task.revision = *next_revision;
            task.status = *status;
            task.pending_interaction_id = pending_interaction_id.clone();
            Ok(())
        }
        TaskPatch::OwnershipChanged {
            task_id,
            expected_revision,
            next_revision,
            backgrounded,
            background_fence,
        } => {
            let Some(task) = snapshot
                .tasks
                .iter_mut()
                .find(|task| task.task_id == *task_id)
            else {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::MissingIdentity,
                    "task does not exist",
                ));
            };
            if task.revision != *expected_revision
                || !task_revision_is_contiguous(*expected_revision, *next_revision)
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "task revision is stale or noncontiguous",
                ));
            }
            if matches!(
                task.status,
                SurfaceTaskStatus::Stopped
                    | SurfaceTaskStatus::Completed
                    | SurfaceTaskStatus::Failed
                    | SurfaceTaskStatus::Cancelled
            ) || (*backgrounded != background_fence.is_some())
            {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task ownership transition is not allowed",
                ));
            }
            task.revision = *next_revision;
            task.backgrounded = *backgrounded;
            task.background_fence = background_fence.clone();
            Ok(())
        }
        TaskPatch::Reconciled {
            source_revision,
            tasks,
        } => {
            let unique_task_ids = tasks
                .iter()
                .map(|task| &task.task_id)
                .collect::<HashSet<_>>()
                .len()
                == tasks.len();
            if !unique_task_ids {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::DuplicateTransition,
                    "task reconciliation contains duplicate identities",
                ));
            }
            if tasks.iter().any(|task| task.revision > *source_revision) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::StaleRevision,
                    "task reconciliation source revision is stale",
                ));
            }
            if snapshot.tasks.iter().any(|current| {
                tasks.iter().find(|next| next.task_id == current.task_id) != Some(current)
            }) {
                return Err(event_error(
                    envelope,
                    SurfaceReducerErrorCode::IllegalTransition,
                    "task reconciliation cannot omit or change existing tasks",
                ));
            }
            for next in tasks {
                let already_present = snapshot
                    .tasks
                    .iter()
                    .any(|current| current.task_id == next.task_id);
                if !already_present
                    && !((next.revision.get() == 1
                        && matches!(
                            next.status,
                            SurfaceTaskStatus::Queued | SurfaceTaskStatus::Running
                        ))
                        || historical_terminal_task_creation_allowed(next))
                {
                    return Err(event_error(
                        envelope,
                        SurfaceReducerErrorCode::IllegalTransition,
                        "task reconciliation creation is not allowed",
                    ));
                }
            }
            validate_task_parent_graph(tasks, envelope)?;
            snapshot.tasks = tasks.clone();
            Ok(())
        }
    }
}

fn goal_patch_id(patch: &GoalPatch) -> &SurfaceGoalId {
    match patch {
        GoalPatch::Created { goal } => &goal.goal_id,
        GoalPatch::Edited { goal_id, .. }
        | GoalPatch::Removed { goal_id, .. }
        | GoalPatch::RunStarted { goal_id, .. }
        | GoalPatch::IntentRequested { goal_id, .. }
        | GoalPatch::IntentAcknowledged { goal_id, .. }
        | GoalPatch::Transitioned { goal_id, .. }
        | GoalPatch::ContinuationDecided { goal_id, .. }
        | GoalPatch::Paused { goal_id, .. }
        | GoalPatch::Recovered { goal_id, .. }
        | GoalPatch::Completed { goal_id, .. } => goal_id,
        GoalPatch::OuterTurnStarted { identity }
        | GoalPatch::OuterTurnFinished { identity, .. }
        | GoalPatch::VerificationCompleted { identity, .. } => &identity.goal_id,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runtime_surface::identity::{
        ContextRevision, DurableRevision, HostIncarnation, HostMonotonicClockId,
        McpCatalogRevision, MonotonicInstant, MonotonicTick, NonEmptyVec, PinnedContextRevision,
        PlanRevision, PolicyEpoch, SessionHealthRevision, SessionMetadataRevision,
        SettingsRevision, SurfaceAdmissionLeaseId, SurfaceBackgroundFence,
        SurfaceBackgroundOwnerToken, SurfaceIncarnation, SurfaceThreadId, UsageRevision,
    };
    use crate::runtime_surface::operation::{
        BusyDisposition, InterruptSettlement, LegacyVisibility, ManualCompactionReason,
        OperationIntent, OperationOrigin, OperationSettingsPreparationReceipt, ReservationLease,
        SurfaceApprovalMode, SurfaceNetworkPermissions, SurfacePermissionRuleSet,
        SurfaceReasoningEffort, SurfaceRuntimeSettings,
    };
    use crate::runtime_surface::projection::{
        ProviderReplayHealth, SurfaceMcpCatalogSnapshot, SurfacePinnedContextSnapshot,
        SurfaceSessionHealth, SurfaceSettingsSnapshot, SurfaceThreadSnapshot, ThreadPersistence,
    };
    use std::collections::BTreeSet;

    pub(crate) fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    #[test]
    fn context_window_identity_advances_only_after_successful_compaction() {
        let operation_id = SurfaceOperationId::try_from_bytes(uuid_v7_bytes(91)).unwrap();
        let window_id = ContextWindowId::new();
        let current = SurfaceContextSnapshot {
            revision: ContextRevision::try_new(1).unwrap(),
            window_id: window_id.clone(),
            used_tokens: 100,
            limit_tokens: 1_000,
            compaction: CompactionState::Running {
                operation_id: operation_id.clone(),
                reason: CompactionReason::Manual,
                before_messages: 10,
            },
            fragments: Vec::new(),
            provider_replay: ProviderReplayHealth::None,
        };
        let mut completed = current.clone();
        completed.revision = ContextRevision::try_new(2).unwrap();
        completed.compaction = CompactionState::Completed {
            operation_id,
            reason: CompactionReason::Manual,
            strategy: NonEmptyText::try_new("remote_summary").unwrap(),
            before_messages: 10,
            after_messages: 4,
            collapsed_messages: 6,
            status_text: DisplayText::new("compacted"),
        };

        assert!(!context_window_transition_valid(&current, &completed));
        completed.window_id = ContextWindowId::new();
        assert!(context_window_transition_valid(&current, &completed));

        let mut usage_update = completed.clone();
        usage_update.revision = ContextRevision::try_new(3).unwrap();
        assert!(context_window_transition_valid(&completed, &usage_update));
        usage_update.window_id = ContextWindowId::new();
        assert!(!context_window_transition_valid(&completed, &usage_update));
    }

    pub(crate) fn thread_id() -> SurfaceThreadId {
        SurfaceThreadId::try_from_bytes([1; 16]).unwrap()
    }

    fn operation_id() -> SurfaceOperationId {
        SurfaceOperationId::try_from_bytes(uuid_v7_bytes(2)).unwrap()
    }

    fn operation_fence() -> SurfaceOperationFence {
        SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: operation_id(),
            generation_id: SurfaceGenerationId::new(0),
        }
    }

    pub(crate) fn digest(seed: u8) -> Sha256Digest {
        Sha256Digest::new([seed; 32])
    }

    fn commit_class() -> CommitClass {
        CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(1).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(3)).unwrap(),
        }
    }

    fn envelope(event: SurfaceEvent) -> SurfaceEventEnvelope {
        SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
            commit_class: commit_class(),
            scope: SurfaceScope::Thread,
            event,
        }
    }

    fn batch(event: SurfaceEventEnvelope) -> SurfaceCommitBatch {
        let cursor_before = SurfaceCursor {
            thread_id: thread_id(),
            incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(5)).unwrap(),
            next_seq: SequenceNumber::new(0),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(1).unwrap(),
            },
        };
        let cursor_after = SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            ..cursor_before.clone()
        };
        SurfaceCommitBatch {
            cursor_before,
            cursor_after,
            commit_class: commit_class(),
            event_count: 1,
            batch_digest: digest(0),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        }
    }

    fn background_event(token: u8) -> SurfaceEventEnvelope {
        envelope(SurfaceEvent::Task(TaskPatch::OwnershipChanged {
            task_id: SurfaceTaskId::try_new("task-1").unwrap(),
            expected_revision: TaskRevision::try_new(1).unwrap(),
            next_revision: TaskRevision::try_new(2).unwrap(),
            backgrounded: true,
            background_fence: Some(SurfaceBackgroundFence {
                operation_fence: operation_fence(),
                background_owner_token: SurfaceBackgroundOwnerToken::new([token; 32]),
            }),
        }))
    }

    fn authority_event(capability_digest: u8) -> SurfaceEventEnvelope {
        let authority = AuthorityFingerprint::new(
            operation_id(),
            digest(10),
            digest(11),
            super::super::identity::test_canonical_path("orca-surface"),
            digest(12),
            PolicyEpoch::try_new(1).unwrap(),
            digest(13),
            digest(14),
            digest(capability_digest),
        );
        envelope(SurfaceEvent::Interaction(InteractionPatch::Requested {
            interaction: SurfaceInteractionView {
                interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(6)).unwrap(),
                revision: InteractionRevision::try_new(1).unwrap(),
                fence: operation_fence(),
                kind: SurfaceInteractionKind::ToolApproval,
                request: SurfaceInteractionRequest::ToolApproval {
                    tool: SurfaceToolRequest {
                        tool_call_id: SurfaceToolCallId::try_new("call-1").unwrap(),
                        source_response_id: None,
                        turn_id: SurfaceTurnId::new(),
                        name: NonEmptyText::try_new("bash").unwrap(),
                        action: SurfaceToolAction::Shell,
                        target: Some(DisplayText::new("cargo test")),
                        raw_arguments: DisplayText::new("{}"),
                        arguments_digest: digest(15),
                    },
                    description: DisplayText::new("run tests"),
                    preview: None,
                    authority,
                },
                route: SurfaceInteractionRoute::Unassigned {
                    epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                },
                lifecycle: SurfaceInteractionLifecycle::Requested,
                recovery_disposition: InteractionUnavailableDisposition::FailOperation,
            },
        }))
    }

    pub(crate) fn reducer_snapshot() -> SurfaceSnapshot {
        let incarnation = SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(5)).unwrap();
        let path = super::super::identity::test_canonical_path("orca-surface");
        let usage = UsageTotals {
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            estimated_cost_usd_micros: 0,
        };
        SurfaceSnapshot {
            cursor: SurfaceCursor {
                thread_id: thread_id(),
                incarnation: incarnation.clone(),
                next_seq: SequenceNumber::new(0),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(1).unwrap(),
                },
            },
            thread: SurfaceThreadSnapshot {
                thread_id: thread_id(),
                owner_epoch: ThreadOwnerEpoch::new(1),
                persistence: ThreadPersistence::RecordedCatalogued,
                title: DisplayText::new("private reducer test"),
                metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
                created_at: UnixMillis::new(1),
                updated_at: UnixMillis::new(1),
                cwd: path.clone(),
                workspace_roots: vec![path.clone()],
                closed: false,
            },
            foreground_operation: None,
            queued_operations: Vec::new(),
            background_operations: Vec::new(),
            operation_history: Vec::new(),
            items: Vec::new(),
            assistant_streams: Vec::new(),
            tools: Vec::new(),
            plan: SurfacePlanSnapshot {
                revision: PlanRevision::try_new(1).unwrap(),
                explanation: None,
                items: Vec::new(),
                causative_generation: None,
            },
            usage: SurfaceUsageSnapshot {
                revision: UsageRevision::try_new(1).unwrap(),
                thread_total: usage,
                active_operation: None,
                goal: None,
                workflow: Vec::new(),
            },
            context: SurfaceContextSnapshot {
                revision: ContextRevision::try_new(1).unwrap(),
                window_id: ContextWindowId::initial_for_thread(&thread_id()),
                used_tokens: 0,
                limit_tokens: 128_000,
                compaction: CompactionState::Idle,
                fragments: Vec::new(),
                provider_replay: ProviderReplayHealth::None,
            },
            interactions: Vec::new(),
            tasks: Vec::new(),
            workflows: Vec::new(),
            subagents: Vec::new(),
            goal: None,
            settings: SurfaceSettingsSnapshot {
                host_revision: SettingsRevision::try_new(1).unwrap(),
                thread_revision: SettingsRevision::try_new(1).unwrap(),
                effective: SurfaceRuntimeSettings {
                    model: NonEmptyText::try_new("deepseek-v4").unwrap(),
                    reasoning_effort: SurfaceReasoningEffort::High,
                    approval_mode: SurfaceApprovalMode::AutoEdit,
                    cwd: path.clone(),
                    workspace_roots: vec![path],
                    active_permission_profile: None,
                    permission_rules: SurfacePermissionRuleSet {
                        ordered_rules: Vec::new(),
                        digest: digest(1),
                    },
                    additional_working_directories: Vec::new(),
                    network_permissions: SurfaceNetworkPermissions {
                        enabled: Some(true),
                        domains: Vec::new(),
                    },
                    policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                },
                pending: None,
                frozen_generation_revision: None,
            },
            mcp_catalog: SurfaceMcpCatalogSnapshot {
                revision: McpCatalogRevision::try_new(1).unwrap(),
                servers: Vec::new(),
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                diagnostics: Vec::new(),
            },
            pinned_context: SurfacePinnedContextSnapshot {
                revision: PinnedContextRevision::try_new(1).unwrap(),
                entries: Vec::new(),
            },
            session_health: SurfaceSessionHealth {
                revision: SessionHealthRevision::try_new(1).unwrap(),
                accepting_admission: true,
                issues: Vec::new(),
                closing: false,
                closed: false,
            },
        }
    }

    fn reducer_batch(
        state: &SurfaceReducerState,
        seed: u8,
        scope: SurfaceScope,
        event: SurfaceEvent,
    ) -> SurfaceCommitBatch {
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(1).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        };
        let envelope = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed.wrapping_add(1))).unwrap(),
            commit_class: commit_class.clone(),
            scope,
            event,
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: state.snapshot.cursor.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(state.snapshot.cursor.next_seq.get() + 1),
                ..state.snapshot.cursor.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: digest(0),
            events: NonEmptyVec::try_new(vec![envelope]).unwrap(),
        };
        batch.batch_digest = canonical_batch_digest(&batch);
        batch
    }

    pub(crate) fn started_operation() -> OperationRecord {
        let fence = operation_fence();
        let incarnation = SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(5)).unwrap();
        let replayability = Replayability::NonReplayable {
            reason: NonReplayableReason::HistoryDisabled,
            live_capsule: LiveOperationCapsule::Available { incarnation },
        };
        OperationRecord {
            operation_id: fence.operation_id.clone(),
            request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(40)).unwrap(),
            intent: OperationIntent {
                origin: OperationOrigin::TuiUser,
                kind: OperationKind::ManualCompaction {
                    reason: ManualCompactionReason::Manual,
                },
                initial_replayability: replayability.clone(),
                busy_disposition: BusyDisposition::Queue,
                interrupt_settlement: InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: LegacyVisibility::PublishAfterAdmitted,
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                required_capabilities: BTreeSet::new(),
                capability_fingerprint: digest(41),
                settings_receipt: OperationSettingsPreparationReceipt::Current {
                    settings_revision: SettingsRevision::try_new(1).unwrap(),
                    policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                },
            },
            phase: OperationPhase::Admitted,
            reservation: ReservationLease::new(
                SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(42)).unwrap(),
                fence.operation_id.clone(),
                SequenceNumber::new(1),
                HostIncarnation::try_from_bytes(uuid_v7_bytes(43)).unwrap(),
                MonotonicInstant {
                    clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(44)).unwrap(),
                    tick: MonotonicTick::new(1),
                },
            ),
            ready_for_admission: true,
            initial_logical_turn_id: Some(SurfaceTurnId::new()),
            initial_input_item_id: None,
            generations: vec![GenerationRecord {
                fence,
                logical_turn_id: SurfaceTurnId::new(),
                input: GenerationInputState::NotApplicable,
                predecessor: None,
                attempt: GenerationAttempt::Initial,
                goal_identity: None,
                replayability,
                required_capabilities: BTreeSet::new(),
                capability_fingerprint: digest(41),
                phase: GenerationPhase::Started,
                started_witness: Some(GenerationStartedWitness {
                    started_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(45)).unwrap(),
                    settings_revision: SettingsRevision::try_new(1).unwrap(),
                    policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                    durable_replayability_digest: digest(46),
                    capability_fingerprint: digest(41),
                }),
                stop_reason: None,
            }],
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        }
    }

    fn resolved_continuation_interaction(
        seed: u8,
    ) -> (
        SurfaceInteractionView,
        SurfaceInteractionResolutionReceipt,
        DurableInteractionContinuationOperationIdentity,
    ) {
        let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(seed)).unwrap();
        let receipt = SurfaceInteractionResolutionReceipt {
            response_id: super::super::identity::SurfaceResponseId::try_from_bytes(uuid_v7_bytes(
                seed + 1,
            ))
            .unwrap(),
            receipt_id: super::super::identity::SurfaceResponseReceiptId::try_from_bytes(
                uuid_v7_bytes(seed + 2),
            )
            .unwrap(),
            kind: SurfaceInteractionKind::UserInput,
            safe_projection: SurfaceInteractionSafeProjection::UserInput { answered: true },
        };
        let identity =
            DurableInteractionContinuationOperationIdentity::try_new(&interaction_id, &receipt)
                .unwrap();
        (
            SurfaceInteractionView {
                interaction_id,
                revision: InteractionRevision::try_new(2).unwrap(),
                fence: operation_fence(),
                kind: SurfaceInteractionKind::UserInput,
                request: SurfaceInteractionRequest::UserInput {
                    question: NonEmptyText::try_new("continue?").unwrap(),
                    suggestions: Vec::new(),
                },
                route: SurfaceInteractionRoute::Unassigned {
                    epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                },
                lifecycle: SurfaceInteractionLifecycle::Resolved {
                    receipt: receipt.clone(),
                },
                recovery_disposition: InteractionUnavailableDisposition::FailOperation,
            },
            receipt,
            identity,
        )
    }

    #[test]
    fn continuation_started_without_atomic_operation_is_rejected() {
        let (interaction, receipt, identity) = resolved_continuation_interaction(90);
        let mut snapshot = reducer_snapshot();
        snapshot.interactions.push(interaction.clone());
        snapshot.operation_history.push(started_operation());
        let state = SurfaceReducerState::new(snapshot);
        let invalid = reducer_batch(
            &state,
            93,
            SurfaceScope::Thread,
            SurfaceEvent::Interaction(InteractionPatch::ContinuationDispatchStarted {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(2).unwrap(),
                next_revision: InteractionRevision::try_new(3).unwrap(),
                receipt_id: receipt.receipt_id,
                dispatch_id: identity.dispatch_id().clone(),
                operation_id: identity.operation_id().clone(),
                turn_id: identity.turn_id().clone(),
            }),
        );

        match reduce_batch(SurfaceReduceMode::Live, &state, &invalid) {
            SurfaceReduceResult::Rejected { error } => {
                assert_eq!(error.code, SurfaceReducerErrorCode::InvalidOrdering)
            }
            _ => panic!("continuation start without operation unexpectedly applied"),
        }
    }

    #[test]
    fn continuation_consumed_before_operation_takeover_is_rejected() {
        let (interaction, receipt, identity) = resolved_continuation_interaction(100);
        let historical_operation = started_operation();
        let mut operation = historical_operation.clone();
        operation.operation_id = identity.operation_id().clone();
        operation.request_id = identity.request_id().clone();
        operation.intent.kind = OperationKind::UserTurn;
        operation.phase = OperationPhase::Admitted;
        operation.initial_logical_turn_id = Some(identity.turn_id().clone());
        operation.agent_loop_turns.clear();
        operation.terminal = None;
        let mut snapshot = reducer_snapshot();
        snapshot.interactions.push(interaction.clone());
        snapshot.operation_history.push(historical_operation);
        snapshot.queued_operations.push(operation);
        let state = SurfaceReducerState::new(snapshot);
        let invalid = reducer_batch(
            &state,
            103,
            SurfaceScope::Thread,
            SurfaceEvent::Interaction(InteractionPatch::ContinuationDispatchConsumed {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(2).unwrap(),
                next_revision: InteractionRevision::try_new(3).unwrap(),
                receipt_id: receipt.receipt_id,
                dispatch_id: identity.dispatch_id().clone(),
                operation_id: identity.operation_id().clone(),
                turn_id: identity.turn_id().clone(),
            }),
        );

        match reduce_batch(SurfaceReduceMode::Live, &state, &invalid) {
            SurfaceReduceResult::Rejected { error } => {
                assert_eq!(error.code, SurfaceReducerErrorCode::InvalidOrdering)
            }
            _ => panic!("continuation consumption before takeover unexpectedly applied"),
        }
    }

    #[test]
    fn assistant_delta_append_counts_only_delta_bytes() {
        let mut text = DisplayText::new("");
        DisplayText::reset_appended_byte_count();
        for _ in 0..1_000 {
            text.push_str("0123456789");
        }
        assert_eq!(text.as_str().len(), 10_000);
        assert_eq!(DisplayText::appended_byte_count(), 10_000);
    }

    #[test]
    fn canonical_digest_covers_background_owner_token() {
        let first_event = background_event(20);
        let second_event = background_event(21);
        assert_ne!(
            canonical_event_digest(&first_event),
            canonical_event_digest(&second_event)
        );

        let first_batch = batch(first_event);
        let second_batch = batch(second_event);
        assert_ne!(
            canonical_batch_digest(&first_batch),
            canonical_batch_digest(&second_batch)
        );
    }

    #[test]
    fn canonical_digest_covers_authority_fingerprint_and_is_stable() {
        let event = authority_event(30);
        let identical = event.clone();
        let changed = authority_event(31);
        assert_eq!(
            canonical_event_digest(&event),
            canonical_event_digest(&identical)
        );
        assert_ne!(
            canonical_event_digest(&event),
            canonical_event_digest(&changed)
        );

        let original_batch = batch(event);
        let identical_batch = batch(identical);
        let changed_batch = batch(changed);
        assert_eq!(
            canonical_batch_digest(&original_batch),
            canonical_batch_digest(&identical_batch)
        );
        assert_ne!(
            canonical_batch_digest(&original_batch),
            canonical_batch_digest(&changed_batch)
        );
        assert_eq!(
            canonical_batch_encoded_bytes(&original_batch),
            canonical_batch_encoded_bytes(&identical_batch)
        );
    }

    #[test]
    fn partial_batch_replay_requires_both_complete_indices() {
        let initial = SurfaceReducerState::new(reducer_snapshot());
        let applied_batch = reducer_batch(
            &initial,
            60,
            SurfaceScope::Thread,
            SurfaceEvent::Plan(SurfacePlanSnapshot {
                revision: PlanRevision::try_new(2).unwrap(),
                explanation: Some(DisplayText::new("partial replay")),
                items: Vec::new(),
                causative_generation: None,
            }),
        );
        let SurfaceReduceResult::Applied { state: applied } =
            reduce_batch(SurfaceReduceMode::Live, &initial, &applied_batch)
        else {
            panic!("expected initial application");
        };
        let commit_id = commit_id(&applied_batch.commit_class).clone();
        let event_id = applied_batch.events.as_slice()[0].event_id.clone();

        let mut missing_event = applied.clone();
        missing_event.applied.remove(&(event_id, commit_id.clone()));
        assert!(matches!(
            reduce_batch(
                SurfaceReduceMode::Rematerialization,
                &missing_event,
                &applied_batch
            ),
            SurfaceReduceResult::Rejected {
                error: SurfaceReducerError {
                    code: SurfaceReducerErrorCode::PartialBatchReplay,
                    location: SurfaceReducerErrorLocation::Batch { .. },
                    ..
                }
            }
        ));

        let mut missing_batch = applied;
        missing_batch.applied_batches.remove(&commit_id);
        assert!(matches!(
            reduce_batch(
                SurfaceReduceMode::Rematerialization,
                &missing_batch,
                &applied_batch
            ),
            SurfaceReduceResult::Rejected {
                error: SurfaceReducerError {
                    code: SurfaceReducerErrorCode::PartialBatchReplay,
                    location: SurfaceReducerErrorLocation::Batch { .. },
                    ..
                }
            }
        ));
    }

    #[test]
    fn reducer_clone_shares_unmodified_applied_history_shards() {
        let initial = SurfaceReducerState::new(reducer_snapshot());
        let batch = reducer_batch(
            &initial,
            61,
            SurfaceScope::Thread,
            SurfaceEvent::Plan(SurfacePlanSnapshot {
                revision: PlanRevision::try_new(2).unwrap(),
                explanation: Some(DisplayText::new("copy on write history")),
                items: Vec::new(),
                causative_generation: None,
            }),
        );

        let SurfaceReduceResult::Applied { state: applied } =
            reduce_batch(SurfaceReduceMode::Live, &initial, &batch)
        else {
            panic!("expected initial application");
        };

        assert!(
            initial.applied.shared_shard_count(&applied.applied) >= APPLIED_HISTORY_SHARD_COUNT - 1
        );
        assert!(
            initial
                .applied_batches
                .shared_shard_count(&applied.applied_batches)
                >= APPLIED_HISTORY_SHARD_COUNT - 1
        );
    }

    #[test]
    fn tool_approval_request_requires_the_exact_persisted_tool_identity() {
        let operation = started_operation();
        let generation = operation.generations[0].clone();
        let persisted_tool = SurfaceToolRequest {
            tool_call_id: SurfaceToolCallId::try_new("persisted-tool").unwrap(),
            source_response_id: None,
            turn_id: generation.logical_turn_id.clone(),
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("cargo test")),
            raw_arguments: DisplayText::new("{}"),
            arguments_digest: digest(51),
        };
        let mut forged_tool = persisted_tool.clone();
        forged_tool.tool_call_id = SurfaceToolCallId::try_new("forged-tool").unwrap();
        let authority = AuthorityFingerprint::new(
            operation.operation_id.clone(),
            digest(52),
            digest(53),
            reducer_snapshot().settings.effective.cwd.clone(),
            digest(54),
            operation.intent.policy_epoch,
            digest(55),
            digest(56),
            generation.capability_fingerprint.clone(),
        );
        let mut snapshot = reducer_snapshot();
        snapshot.foreground_operation = Some(operation);
        snapshot.tools.push(SurfaceToolView {
            request: persisted_tool,
            state: SurfaceToolViewState::Running,
            invocation_started: None,
            arguments_bytes: ByteCount::new(2),
            output_bytes: ByteCount::new(0),
            streamed_output: DisplayText::new(""),
            streamed_output_truncated: false,
            result: None,
            capability_calls: Vec::new(),
            terminal_leases: Vec::new(),
        });
        let initial = SurfaceReducerState::new(snapshot);
        let invalid = reducer_batch(
            &initial,
            61,
            SurfaceScope::Generation {
                fence: generation.fence.clone(),
            },
            SurfaceEvent::Interaction(InteractionPatch::Requested {
                interaction: SurfaceInteractionView {
                    interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(62))
                        .unwrap(),
                    revision: InteractionRevision::try_new(1).unwrap(),
                    fence: generation.fence,
                    kind: SurfaceInteractionKind::ToolApproval,
                    request: SurfaceInteractionRequest::ToolApproval {
                        tool: forged_tool,
                        description: DisplayText::new("run tests"),
                        preview: None,
                        authority,
                    },
                    route: SurfaceInteractionRoute::Unassigned {
                        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                    },
                    lifecycle: SurfaceInteractionLifecycle::Requested,
                    recovery_disposition: InteractionUnavailableDisposition::FailOperation,
                },
            }),
        );

        assert!(matches!(
            reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            SurfaceReduceResult::Rejected {
                error: SurfaceReducerError {
                    code: SurfaceReducerErrorCode::IllegalTransition,
                    ..
                }
            }
        ));
    }

    #[test]
    fn tool_approval_rejects_a_wrong_executable_authority_generation() {
        let mut operation = started_operation();
        let request_digest = digest(57);
        let tool_schema_digest = digest(58);
        let cwd = reducer_snapshot().settings.effective.cwd.clone();
        let workspace_roots = vec![cwd.clone()];
        let replayability = Replayability::Replayable {
            capsule_digest: digest(59),
            request: None,
            request_digest: Some(request_digest.clone()),
            cwd: cwd.clone(),
            workspace_roots: workspace_roots.clone(),
            settings_revision: SettingsRevision::try_new(1).unwrap(),
            policy_epoch: operation.intent.policy_epoch,
            tool_schema_digest: tool_schema_digest.clone(),
        };
        operation.intent.initial_replayability = replayability.clone();
        operation.generations[0].replayability = replayability;
        let generation = operation.generations[0].clone();
        let tool = SurfaceToolRequest {
            tool_call_id: SurfaceToolCallId::try_new("persisted-effect").unwrap(),
            source_response_id: None,
            turn_id: generation.logical_turn_id.clone(),
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("cargo test")),
            raw_arguments: DisplayText::new("{}"),
            arguments_digest: digest(60),
        };
        let authority = AuthorityFingerprint::new(
            operation.operation_id.clone(),
            request_digest,
            tool_schema_digest,
            cwd,
            sha256(&serde_json::to_vec(&workspace_roots).unwrap()),
            operation.intent.policy_epoch,
            digest(61),
            tool.arguments_digest.clone(),
            generation.capability_fingerprint.clone(),
        );
        let mut snapshot = reducer_snapshot();
        snapshot.foreground_operation = Some(operation);
        snapshot.tools.push(SurfaceToolView {
            request: tool.clone(),
            state: SurfaceToolViewState::Requested,
            invocation_started: None,
            arguments_bytes: ByteCount::new(2),
            output_bytes: ByteCount::new(0),
            streamed_output: DisplayText::new(""),
            streamed_output_truncated: false,
            result: None,
            capability_calls: Vec::new(),
            terminal_leases: Vec::new(),
        });
        let initial = SurfaceReducerState::new(snapshot);
        let invalid = reducer_batch(
            &initial,
            63,
            SurfaceScope::Generation {
                fence: generation.fence.clone(),
            },
            SurfaceEvent::Interaction(InteractionPatch::Requested {
                interaction: SurfaceInteractionView {
                    interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(64))
                        .unwrap(),
                    revision: InteractionRevision::try_new(1).unwrap(),
                    fence: generation.fence,
                    kind: SurfaceInteractionKind::ToolApproval,
                    request: SurfaceInteractionRequest::ToolApproval {
                        tool,
                        description: DisplayText::new("run tests"),
                        preview: None,
                        authority,
                    },
                    route: SurfaceInteractionRoute::Unassigned {
                        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                    },
                    lifecycle: SurfaceInteractionLifecycle::Requested,
                    recovery_disposition: InteractionUnavailableDisposition::FailOperation,
                },
            }),
        );

        assert!(matches!(
            reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            SurfaceReduceResult::Rejected {
                error: SurfaceReducerError {
                    code: SurfaceReducerErrorCode::IllegalTransition,
                    ..
                }
            }
        ));
    }

    #[test]
    fn generation_transfer_preserves_record_and_background_stop_absorbs() {
        let operation = started_operation();
        let fence = operation.generations[0].fence.clone();
        let mut snapshot = reducer_snapshot();
        snapshot.foreground_operation = Some(operation);
        let initial = SurfaceReducerState::new(snapshot);
        let background_fence = SurfaceBackgroundFence {
            operation_fence: fence.clone(),
            background_owner_token: SurfaceBackgroundOwnerToken::new([47; 32]),
        };
        let transfer = reducer_batch(
            &initial,
            70,
            SurfaceScope::Generation {
                fence: fence.clone(),
            },
            SurfaceEvent::Operation(OperationPatch::GenerationTransferred {
                fence: fence.clone(),
                background_fence: background_fence.clone(),
                task_id: None,
            }),
        );
        let SurfaceReduceResult::Applied { state: transferred } =
            reduce_batch(SurfaceReduceMode::Live, &initial, &transfer)
        else {
            panic!("expected generation transfer");
        };
        assert!(transferred.snapshot.foreground_operation.is_none());
        assert_eq!(transferred.snapshot.background_operations.len(), 1);
        assert_eq!(transferred.snapshot.operation_history.len(), 1);
        assert_eq!(
            transferred.snapshot.operation_history[0].generations[0].phase,
            GenerationPhase::Transferred
        );

        let stale_owner = reducer_batch(
            &transferred,
            71,
            SurfaceScope::Background {
                fence: SurfaceBackgroundFence {
                    operation_fence: fence.clone(),
                    background_owner_token: SurfaceBackgroundOwnerToken::new([48; 32]),
                },
            },
            SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                fence: fence.clone(),
                reason: GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                },
                usage_delta: UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
            }),
        );
        assert!(matches!(
            reduce_batch(SurfaceReduceMode::Live, &transferred, &stale_owner),
            SurfaceReduceResult::Rejected {
                error: SurfaceReducerError {
                    code: SurfaceReducerErrorCode::ScopeMismatch,
                    ..
                }
            }
        ));
        assert_eq!(
            transferred.snapshot.operation_history[0].generations[0].phase,
            GenerationPhase::Transferred
        );

        let stop = reducer_batch(
            &transferred,
            72,
            SurfaceScope::Background {
                fence: background_fence.clone(),
            },
            SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                fence: fence.clone(),
                reason: GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                },
                usage_delta: UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
            }),
        );
        let SurfaceReduceResult::Applied { state: stopped } =
            reduce_batch(SurfaceReduceMode::Live, &transferred, &stop)
        else {
            panic!("expected background generation stop");
        };
        assert_eq!(
            stopped.snapshot.operation_history[0].generations[0].phase,
            GenerationPhase::Stopped
        );
        assert_eq!(stopped.snapshot.background_operations.len(), 1);

        let finalize_intent_id =
            SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(73)).unwrap();
        let finalizing = reducer_batch(
            &stopped,
            74,
            SurfaceScope::Background {
                fence: background_fence.clone(),
            },
            SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                operation_id: fence.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(76)).unwrap(),
                selected_cause: OperationFinalizationCause::GenerationStop(
                    GenerationStopReason::Completed {
                        status: GenerationCompletionStatus::Success,
                    },
                ),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        );
        let SurfaceReduceResult::Applied { state: finalizing } =
            reduce_batch(SurfaceReduceMode::Live, &stopped, &finalizing)
        else {
            panic!("expected background finalization start");
        };
        let terminal = reducer_batch(
            &finalizing,
            76,
            SurfaceScope::Background {
                fence: background_fence,
            },
            SurfaceEvent::Operation(OperationPatch::Terminal {
                record: OperationTerminalRecord {
                    operation_id: fence.operation_id,
                    finalize_intent_id,
                    terminal: OperationTerminal::Succeeded {
                        usage: UsageTotals {
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_tokens: 0,
                            estimated_cost_usd_micros: 0,
                        },
                    },
                    usage: UsageTotals {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_tokens: 0,
                        estimated_cost_usd_micros: 0,
                    },
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    completion_proof: super::super::SurfaceOperationCompletionProof::unverified(
                        "test terminal has no verifier proof",
                    ),
                    committed_at: UnixMillis::new(5),
                },
            }),
        );
        let SurfaceReduceResult::Applied { state: terminal } =
            reduce_batch(SurfaceReduceMode::Live, &finalizing, &terminal)
        else {
            panic!("expected background terminal");
        };
        assert!(terminal.snapshot.background_operations.is_empty());
        assert_eq!(terminal.snapshot.operation_history.len(), 1);
        assert!(matches!(
            terminal.snapshot.operation_history[0].phase,
            OperationPhase::Terminal
        ));
        assert!(terminal.snapshot.operation_history[0].terminal.is_some());
    }
}
