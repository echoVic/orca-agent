use super::commands::{
    ClosedThreadReceipt, EphemeralThreadPersistence, HostReceiptAckRequirement,
    HostReceiptIdentityPair, HostReceiptRequirementIdentity, MutationCommitAck,
    OperationTerminalAckRequirement, OperationTerminalAtCursor, RetainedShutdownOutput,
    ShutdownBarrierPlan, ShutdownBarrierRecord, ShutdownBarrierState, ShutdownHostOutput,
    ShutdownOperationPlan, ShutdownOperationSourcePhase, ShutdownRequestCause,
    ShutdownSelectedCause, ShutdownThreadPlan, SurfaceCommitBatch,
    SurfaceCommitBatchPreflightResult, SurfaceEvent, SurfaceEventEnvelope,
    SurfaceHostShutdownReceipt, SurfaceHostShutdownStage, SurfaceSessionCatalogAction,
    SurfaceSessionCatalogReceipt, ThreadCursorAckRequirement,
};
use super::commit::ImmutableShutdownLedger;
use super::identity::{
    CanonicalPath, CommitClass, CursorSourceRevision, DisplayText, DurableRevision,
    HostIncarnation, HostLifecycleRevision, InteractionRevision, LiveRevision, NonEmptyText,
    NonEmptyVec, PolicyEpoch, SafeDiagnosticText, SequenceNumber, SessionCatalogRevision,
    Sha256Digest, SurfaceBackgroundFence, SurfaceBackgroundOwnerToken, SurfaceCommitId,
    SurfaceCursor, SurfaceEventId, SurfaceFinalizeIntentId, SurfaceGenerationId, SurfaceGoalId,
    SurfaceInteractionId, SurfaceItemId, SurfaceOperationFence, SurfaceOperationId,
    SurfaceRequestId, SurfaceScope, SurfaceSettlementId, SurfaceSubagentId, SurfaceTaskFence,
    SurfaceTaskId, SurfaceThreadId, SurfaceToolCallId, SurfaceTurnId, SurfaceWorkflowRunId,
    TaskRevision, ThreadOwnerEpoch, UnixMillis, UuidV7, canonical_background_fence_v1,
};
use super::interaction::{
    AuthorityFingerprint, DurableInteractionContinuationAnswer, InteractionCancelReason,
    InteractionExpiryDeadline, InteractionPatch, InteractionUnavailableDisposition,
    SurfaceInteractionKind, SurfaceInteractionLifecycle, SurfaceInteractionRequest,
    SurfaceInteractionResolutionReceipt, SurfaceInteractionRoute, SurfaceInteractionView,
    SurfaceMcpElicitationRequest, SurfacePermissionContext, SurfacePermissionProfile,
    SurfaceToolRequest,
};
#[cfg(test)]
use super::operation::GenerationExecutionFailureClass;
use super::operation::{
    AdmittedInput, FinalizationDegradedCause, GenerationRecord, GenerationStartedWitness,
    GenerationStopReason, InputResolutionErrorCode, OperationFinalizationCause, OperationRecord,
    OperationTerminal, OperationTerminalRecord, PendingControlIntent, SurfaceAgentLoopTurn,
    SurfaceResolvedInputFact, SurfaceSettlementReceipt, SuspendedFinalizationCause,
    SuspensionCause, UsageTotals,
};
use super::projection::{
    AssistantPatch, FirstOperationCompletionPolicy, GoalPatchEnvelope, ItemPatch, McpCatalogPatch,
    OperationPatch, PinnedContextPatch, SessionPatch, SettingsPatch, SubagentPatch,
    SurfaceContextSnapshot, SurfaceFactFamily, SurfacePlanSnapshot, SurfaceTask, SurfaceTaskStatus,
    SurfaceTaskType, SurfaceUsageSnapshot, SurfaceVerificationResult, TaskPatch, ToolPatch,
    WorkflowPatch,
};
#[cfg(test)]
use super::projection::{
    CompactionReason, CompactionState, SurfaceCapabilityCall, SurfaceCapabilityCallKind,
    SurfaceCapabilityCallState,
};
use super::reducer::{canonical_batch_digest, preflight_batch};
use crate::thread_store::JsonlThreadStore;
use orca_platform::PlatformError;
use orca_platform::fs::ExclusiveFileLock;
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceLedgerError {
    AppendFailed,
    PartialAppend,
    CheckpointFailed,
    CommitIdentityConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableBatchReceipt {
    pub commit_id: SurfaceCommitId,
    pub durable_revision: DurableRevision,
    pub event_count: u32,
    pub batch_digest: Sha256Digest,
    pub cursor_after: SurfaceCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EphemeralBatchReceipt {
    pub commit_id: SurfaceCommitId,
    pub live_revision: LiveRevision,
    pub event_count: u32,
    pub batch_digest: Sha256Digest,
    pub cursor_after: SurfaceCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceBatchReceipt {
    Recorded(DurableBatchReceipt),
    Ephemeral(EphemeralBatchReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSurfaceCommit {
    pub commit_id: SurfaceCommitId,
    pub event_count: u32,
    pub batch_digest: Sha256Digest,
    pub cursor_before: SurfaceCursor,
    pub cursor_after: SurfaceCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitProbe {
    Absent,
    Prepared(PreparedSurfaceCommit),
    Present(SurfaceBatchReceipt),
    Conflict,
}

pub trait SurfaceCommitLedger {
    fn append_complete_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError>;

    fn checkpoint(&mut self, receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError>;

    fn probe_commit(&self, commit_id: &SurfaceCommitId, digest: &Sha256Digest) -> CommitProbe;

    /// Returns the durable receipt for a commit id without requiring the
    /// caller to know the batch digest. This is used to make command retries
    /// idempotent after a lost actor reply; implementations that cannot
    /// provide an index may conservatively return `None`.
    fn lookup_commit(&self, _commit_id: &SurfaceCommitId) -> Option<SurfaceBatchReceipt> {
        None
    }

    /// Returns the source digest persisted by a committed subagent event.
    /// Keeping this lookup on the ledger makes retry identity survive actor
    /// restart instead of relying on a process-local dedupe cache.
    fn lookup_subagent_source_digest(&self, _commit_id: &SurfaceCommitId) -> Option<Sha256Digest> {
        None
    }
}

fn subagent_source_digest(batch: &SurfaceCommitBatch) -> Option<Sha256Digest> {
    batch
        .events
        .as_slice()
        .iter()
        .find_map(|event| match &event.event {
            SurfaceEvent::Subagent(SubagentPatch::Started { subagent, .. }) => {
                Some(subagent.as_subagent().source.source_digest)
            }
            SurfaceEvent::Subagent(SubagentPatch::Progress { source, .. })
            | SurfaceEvent::Subagent(SubagentPatch::Completed { source, .. }) => {
                Some(source.source_digest)
            }
            SurfaceEvent::Subagent(SubagentPatch::Stopped { .. }) => None,
            _ => None,
        })
}

pub struct InMemorySurfaceCommitLedger {
    cursor: SurfaceCursor,
    receipts: BTreeMap<SurfaceCommitId, EphemeralBatchReceipt>,
    committed: Vec<SurfaceCommitBatch>,
}

impl InMemorySurfaceCommitLedger {
    pub fn new(cursor: SurfaceCursor) -> Self {
        Self {
            cursor,
            receipts: BTreeMap::new(),
            committed: Vec::new(),
        }
    }

    pub fn recover_batches(&self) -> RecoveredSurfaceBatches {
        RecoveredSurfaceBatches {
            committed: self.committed.clone(),
            prepared: None,
        }
    }

    fn receipt_for(
        &self,
        commit_id: &SurfaceCommitId,
        digest: &Sha256Digest,
    ) -> Result<Option<EphemeralBatchReceipt>, SurfaceLedgerError> {
        let Some(receipt) = self.receipts.get(commit_id) else {
            return Ok(None);
        };
        if &receipt.batch_digest != digest {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        }
        Ok(Some(receipt.clone()))
    }
}

impl SurfaceCommitLedger for InMemorySurfaceCommitLedger {
    fn append_complete_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError> {
        let (incarnation, live_revision, commit_id) = match &batch.commit_class {
            CommitClass::Ephemeral {
                incarnation,
                live_revision,
                commit_id,
            } => (incarnation, *live_revision, commit_id),
            CommitClass::Recorded { .. } => {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
        };
        if let Some(receipt) = self.receipt_for(commit_id, &batch.batch_digest)? {
            return (receipt.cursor_after == batch.cursor_after
                && receipt.event_count == batch.event_count)
                .then_some(SurfaceBatchReceipt::Ephemeral(receipt))
                .ok_or(SurfaceLedgerError::CommitIdentityConflict);
        }
        let next_live_revision = match self.cursor.source_revision {
            CursorSourceRevision::Ephemeral { live_revision } => LiveRevision::try_new(
                live_revision
                    .get()
                    .checked_add(1)
                    .ok_or(SurfaceLedgerError::CommitIdentityConflict)?,
            )
            .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)?,
            CursorSourceRevision::Recorded { .. } => {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
        };
        let cursor_revision_matches = matches!(
            batch.cursor_after.source_revision,
            CursorSourceRevision::Ephemeral { live_revision: cursor_revision }
                if cursor_revision == live_revision
        );
        let envelopes_match = batch.events.as_slice().iter().all(|event| {
            event.commit_class == batch.commit_class && event.ordinal < batch.event_count
        });
        if batch.cursor_before != self.cursor
            || batch.cursor_after.thread_id != self.cursor.thread_id
            || batch.cursor_after.incarnation != self.cursor.incarnation
            || incarnation != &self.cursor.incarnation
            || live_revision != next_live_revision
            || !cursor_revision_matches
            || batch.cursor_after.next_seq.get()
                != self
                    .cursor
                    .next_seq
                    .get()
                    .checked_add(u64::from(batch.event_count))
                    .ok_or(SurfaceLedgerError::CommitIdentityConflict)?
            || batch.event_count != batch.events.as_slice().len() as u32
            || !envelopes_match
            || canonical_batch_digest(batch) != batch.batch_digest
        {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        }
        let receipt = EphemeralBatchReceipt {
            commit_id: commit_id.clone(),
            live_revision,
            event_count: batch.event_count,
            batch_digest: batch.batch_digest.clone(),
            cursor_after: batch.cursor_after.clone(),
        };
        self.cursor = batch.cursor_after.clone();
        self.receipts.insert(commit_id.clone(), receipt.clone());
        self.committed.push(batch.clone());
        Ok(SurfaceBatchReceipt::Ephemeral(receipt))
    }

    fn checkpoint(&mut self, receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError> {
        let SurfaceBatchReceipt::Ephemeral(receipt) = receipt else {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        };
        self.receipt_for(&receipt.commit_id, &receipt.batch_digest)?
            .filter(|stored| stored == receipt)
            .map(|_| ())
            .ok_or(SurfaceLedgerError::CommitIdentityConflict)
    }

    fn probe_commit(&self, commit_id: &SurfaceCommitId, digest: &Sha256Digest) -> CommitProbe {
        match self.receipt_for(commit_id, digest) {
            Ok(Some(receipt)) => CommitProbe::Present(SurfaceBatchReceipt::Ephemeral(receipt)),
            Ok(None) => CommitProbe::Absent,
            Err(_) => CommitProbe::Conflict,
        }
    }

    fn lookup_commit(&self, commit_id: &SurfaceCommitId) -> Option<SurfaceBatchReceipt> {
        self.receipts
            .get(commit_id)
            .cloned()
            .map(SurfaceBatchReceipt::Ephemeral)
    }

    fn lookup_subagent_source_digest(&self, commit_id: &SurfaceCommitId) -> Option<Sha256Digest> {
        self.committed
            .iter()
            .find(|batch| SurfaceCommitIndex::commit_id(batch) == commit_id)
            .and_then(subagent_source_digest)
    }
}

pub(crate) enum RuntimeSurfaceCommitLedger {
    Recorded(JsonlSurfaceCommitLedger),
    Ephemeral(InMemorySurfaceCommitLedger),
}

impl RuntimeSurfaceCommitLedger {
    pub(crate) fn recover_batches(&self) -> Result<RecoveredSurfaceBatches, SurfaceLedgerError> {
        match self {
            Self::Recorded(ledger) => ledger.recover_batches(),
            Self::Ephemeral(ledger) => Ok(ledger.recover_batches()),
        }
    }
}

impl SurfaceCommitLedger for RuntimeSurfaceCommitLedger {
    fn append_complete_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError> {
        match self {
            Self::Recorded(ledger) => ledger.append_complete_batch(batch),
            Self::Ephemeral(ledger) => ledger.append_complete_batch(batch),
        }
    }

    fn checkpoint(&mut self, receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError> {
        match self {
            Self::Recorded(ledger) => ledger.checkpoint(receipt),
            Self::Ephemeral(ledger) => ledger.checkpoint(receipt),
        }
    }

    fn probe_commit(&self, commit_id: &SurfaceCommitId, digest: &Sha256Digest) -> CommitProbe {
        match self {
            Self::Recorded(ledger) => ledger.probe_commit(commit_id, digest),
            Self::Ephemeral(ledger) => ledger.probe_commit(commit_id, digest),
        }
    }

    fn lookup_commit(&self, commit_id: &SurfaceCommitId) -> Option<SurfaceBatchReceipt> {
        match self {
            Self::Recorded(ledger) => ledger.lookup_commit(commit_id),
            Self::Ephemeral(ledger) => ledger.lookup_commit(commit_id),
        }
    }

    fn lookup_subagent_source_digest(&self, commit_id: &SurfaceCommitId) -> Option<Sha256Digest> {
        match self {
            Self::Recorded(ledger) => ledger.lookup_subagent_source_digest(commit_id),
            Self::Ephemeral(ledger) => ledger.lookup_subagent_source_digest(commit_id),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct RecoveredSurfaceBatches {
    pub committed: Vec<SurfaceCommitBatch>,
    pub prepared: Option<SurfaceCommitBatch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredBackgroundFenceV1 {
    operation_fence: SurfaceOperationFence,
    background_owner_token: [u8; 32],
}

impl StoredBackgroundFenceV1 {
    fn from_live(fence: &SurfaceBackgroundFence) -> Result<Self, SurfaceLedgerError> {
        let value = serde_json::to_value(canonical_background_fence_v1(fence))
            .map_err(|_| SurfaceLedgerError::AppendFailed)?;
        serde_json::from_value(value).map_err(|_| SurfaceLedgerError::AppendFailed)
    }

    fn into_live(self) -> SurfaceBackgroundFence {
        SurfaceBackgroundFence {
            operation_fence: self.operation_fence,
            background_owner_token: SurfaceBackgroundOwnerToken::new(self.background_owner_token),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredScopeV1 {
    Thread,
    Operation {
        operation_id: SurfaceOperationId,
    },
    Generation {
        fence: SurfaceOperationFence,
    },
    Background {
        fence: StoredBackgroundFenceV1,
    },
    Goal {
        goal_id: SurfaceGoalId,
        causative_generation: Option<SurfaceOperationFence>,
    },
}

impl StoredScopeV1 {
    fn from_live(scope: &SurfaceScope) -> Result<Self, SurfaceLedgerError> {
        Ok(match scope {
            SurfaceScope::Thread => Self::Thread,
            SurfaceScope::Operation { operation_id } => Self::Operation {
                operation_id: operation_id.clone(),
            },
            SurfaceScope::Generation { fence } => Self::Generation {
                fence: fence.clone(),
            },
            SurfaceScope::Background { fence } => Self::Background {
                fence: StoredBackgroundFenceV1::from_live(fence)?,
            },
            SurfaceScope::Goal {
                goal_id,
                causative_generation,
            } => Self::Goal {
                goal_id: goal_id.clone(),
                causative_generation: causative_generation.clone(),
            },
        })
    }

    fn into_live(self) -> SurfaceScope {
        match self {
            Self::Thread => SurfaceScope::Thread,
            Self::Operation { operation_id } => SurfaceScope::Operation { operation_id },
            Self::Generation { fence } => SurfaceScope::Generation { fence },
            Self::Background { fence } => SurfaceScope::Background {
                fence: fence.into_live(),
            },
            Self::Goal {
                goal_id,
                causative_generation,
            } => SurfaceScope::Goal {
                goal_id,
                causative_generation,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredOperationPatchV1 {
    Requested {
        operation: OperationRecord,
    },
    ReservationQueueChanged {
        operation_id: SurfaceOperationId,
        reservation_sequence: SequenceNumber,
        ready_for_admission: bool,
        queue_position: u32,
    },
    Admitted {
        operation_id: SurfaceOperationId,
        logical_turn_id: SurfaceTurnId,
        input: AdmittedInput,
        first_generation: GenerationRecord,
    },
    InputBindingsResolved {
        fence: SurfaceOperationFence,
        input_item_id: SurfaceItemId,
        fact: SurfaceResolvedInputFact,
    },
    InputBindingsFailed {
        fence: SurfaceOperationFence,
        input_item_id: SurfaceItemId,
        code: InputResolutionErrorCode,
        message: SafeDiagnosticText,
    },
    ControlIntentCommitted {
        operation_id: SurfaceOperationId,
        request_id: SurfaceRequestId,
        intent: PendingControlIntent,
    },
    GenerationReserved {
        generation: GenerationRecord,
    },
    GenerationStarted {
        fence: SurfaceOperationFence,
        witness: GenerationStartedWitness,
    },
    AgentLoopTurnStarted {
        turn: SurfaceAgentLoopTurn,
    },
    ModelRouteSelected {
        fence: SurfaceOperationFence,
        requested_model: NonEmptyText,
        actual_model: NonEmptyText,
        reason: NonEmptyText,
    },
    VerificationStarted {
        fence: SurfaceOperationFence,
        verification_id: UuidV7,
        command: NonEmptyText,
    },
    VerificationCompleted {
        fence: SurfaceOperationFence,
        verification_id: UuidV7,
        result: SurfaceVerificationResult,
    },
    GenerationStopped {
        fence: SurfaceOperationFence,
        reason: GenerationStopReason,
        usage_delta: UsageTotals,
    },
    GenerationTransferred {
        fence: SurfaceOperationFence,
        background_fence: StoredBackgroundFenceV1,
        task_id: Option<SurfaceTaskId>,
    },
    Suspended {
        operation_id: SurfaceOperationId,
        cause: SuspensionCause,
    },
    SuspensionRebasedAfterUnstartedResume {
        operation_id: SurfaceOperationId,
        previous_cause: SuspensionCause,
        replacement_fence: SurfaceOperationFence,
        rebased_cause: SuspensionCause,
    },
    RecoveryRequired {
        operation_id: SurfaceOperationId,
        last_generation: SurfaceGenerationId,
    },
    FinalizationStarted {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        selected_cause: OperationFinalizationCause,
        suspended_cause: Option<SuspendedFinalizationCause>,
        expected_settlements: Vec<SurfaceSettlementId>,
    },
    FinalizationSettlementRecorded {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        receipt: SurfaceSettlementReceipt,
    },
    FinalizationDegraded {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        cause: FinalizationDegradedCause,
        last_error: DisplayText,
    },
    Terminal {
        record: OperationTerminalRecord,
    },
}

impl StoredOperationPatchV1 {
    fn from_live(patch: &OperationPatch) -> Result<Self, SurfaceLedgerError> {
        Ok(match patch {
            OperationPatch::Requested { operation } => Self::Requested {
                operation: operation.clone(),
            },
            OperationPatch::ReservationQueueChanged {
                operation_id,
                reservation_sequence,
                ready_for_admission,
                queue_position,
            } => Self::ReservationQueueChanged {
                operation_id: operation_id.clone(),
                reservation_sequence: *reservation_sequence,
                ready_for_admission: *ready_for_admission,
                queue_position: *queue_position,
            },
            OperationPatch::Admitted {
                operation_id,
                logical_turn_id,
                input,
                first_generation,
            } => Self::Admitted {
                operation_id: operation_id.clone(),
                logical_turn_id: logical_turn_id.clone(),
                input: input.clone(),
                first_generation: first_generation.clone(),
            },
            OperationPatch::InputBindingsResolved {
                fence,
                input_item_id,
                fact,
            } => Self::InputBindingsResolved {
                fence: fence.clone(),
                input_item_id: input_item_id.clone(),
                fact: fact.clone(),
            },
            OperationPatch::InputBindingsFailed {
                fence,
                input_item_id,
                code,
                message,
            } => Self::InputBindingsFailed {
                fence: fence.clone(),
                input_item_id: input_item_id.clone(),
                code: *code,
                message: message.clone(),
            },
            OperationPatch::ControlIntentCommitted {
                operation_id,
                request_id,
                intent,
            } => Self::ControlIntentCommitted {
                operation_id: operation_id.clone(),
                request_id: request_id.clone(),
                intent: intent.clone(),
            },
            OperationPatch::GenerationReserved { generation } => Self::GenerationReserved {
                generation: generation.clone(),
            },
            OperationPatch::GenerationStarted { fence, witness } => Self::GenerationStarted {
                fence: fence.clone(),
                witness: witness.clone(),
            },
            OperationPatch::AgentLoopTurnStarted { turn } => {
                Self::AgentLoopTurnStarted { turn: turn.clone() }
            }
            OperationPatch::ModelRouteSelected {
                fence,
                requested_model,
                actual_model,
                reason,
            } => Self::ModelRouteSelected {
                fence: fence.clone(),
                requested_model: requested_model.clone(),
                actual_model: actual_model.clone(),
                reason: reason.clone(),
            },
            OperationPatch::VerificationStarted {
                fence,
                verification_id,
                command,
            } => Self::VerificationStarted {
                fence: fence.clone(),
                verification_id: verification_id.clone(),
                command: command.clone(),
            },
            OperationPatch::VerificationCompleted {
                fence,
                verification_id,
                result,
            } => Self::VerificationCompleted {
                fence: fence.clone(),
                verification_id: verification_id.clone(),
                result: result.clone(),
            },
            OperationPatch::GenerationStopped {
                fence,
                reason,
                usage_delta,
            } => Self::GenerationStopped {
                fence: fence.clone(),
                reason: reason.clone(),
                usage_delta: usage_delta.clone(),
            },
            OperationPatch::GenerationTransferred {
                fence,
                background_fence,
                task_id,
            } => Self::GenerationTransferred {
                fence: fence.clone(),
                background_fence: StoredBackgroundFenceV1::from_live(background_fence)?,
                task_id: task_id.clone(),
            },
            OperationPatch::Suspended {
                operation_id,
                cause,
            } => Self::Suspended {
                operation_id: operation_id.clone(),
                cause: cause.clone(),
            },
            OperationPatch::SuspensionRebasedAfterUnstartedResume {
                operation_id,
                previous_cause,
                replacement_fence,
                rebased_cause,
            } => Self::SuspensionRebasedAfterUnstartedResume {
                operation_id: operation_id.clone(),
                previous_cause: previous_cause.clone(),
                replacement_fence: replacement_fence.clone(),
                rebased_cause: rebased_cause.clone(),
            },
            OperationPatch::RecoveryRequired {
                operation_id,
                last_generation,
            } => Self::RecoveryRequired {
                operation_id: operation_id.clone(),
                last_generation: *last_generation,
            },
            OperationPatch::FinalizationStarted {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                selected_cause,
                suspended_cause,
                expected_settlements,
            } => Self::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: selected_cause.clone(),
                suspended_cause: suspended_cause.clone(),
                expected_settlements: expected_settlements.clone(),
            },
            OperationPatch::FinalizationSettlementRecorded {
                operation_id,
                finalize_intent_id,
                receipt,
            } => Self::FinalizationSettlementRecorded {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                receipt: receipt.clone(),
            },
            OperationPatch::FinalizationDegraded {
                operation_id,
                finalize_intent_id,
                cause,
                last_error,
            } => Self::FinalizationDegraded {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                cause: cause.clone(),
                last_error: last_error.clone(),
            },
            OperationPatch::Terminal { record } => Self::Terminal {
                record: record.clone(),
            },
        })
    }

    fn into_live(self) -> OperationPatch {
        match self {
            Self::Requested { operation } => OperationPatch::Requested { operation },
            Self::ReservationQueueChanged {
                operation_id,
                reservation_sequence,
                ready_for_admission,
                queue_position,
            } => OperationPatch::ReservationQueueChanged {
                operation_id,
                reservation_sequence,
                ready_for_admission,
                queue_position,
            },
            Self::Admitted {
                operation_id,
                logical_turn_id,
                input,
                first_generation,
            } => OperationPatch::Admitted {
                operation_id,
                logical_turn_id,
                input,
                first_generation,
            },
            Self::InputBindingsResolved {
                fence,
                input_item_id,
                fact,
            } => OperationPatch::InputBindingsResolved {
                fence,
                input_item_id,
                fact,
            },
            Self::InputBindingsFailed {
                fence,
                input_item_id,
                code,
                message,
            } => OperationPatch::InputBindingsFailed {
                fence,
                input_item_id,
                code,
                message,
            },
            Self::ControlIntentCommitted {
                operation_id,
                request_id,
                intent,
            } => OperationPatch::ControlIntentCommitted {
                operation_id,
                request_id,
                intent,
            },
            Self::GenerationReserved { generation } => {
                OperationPatch::GenerationReserved { generation }
            }
            Self::GenerationStarted { fence, witness } => {
                OperationPatch::GenerationStarted { fence, witness }
            }
            Self::AgentLoopTurnStarted { turn } => OperationPatch::AgentLoopTurnStarted { turn },
            Self::ModelRouteSelected {
                fence,
                requested_model,
                actual_model,
                reason,
            } => OperationPatch::ModelRouteSelected {
                fence,
                requested_model,
                actual_model,
                reason,
            },
            Self::VerificationStarted {
                fence,
                verification_id,
                command,
            } => OperationPatch::VerificationStarted {
                fence,
                verification_id,
                command,
            },
            Self::VerificationCompleted {
                fence,
                verification_id,
                result,
            } => OperationPatch::VerificationCompleted {
                fence,
                verification_id,
                result,
            },
            Self::GenerationStopped {
                fence,
                reason,
                usage_delta,
            } => OperationPatch::GenerationStopped {
                fence,
                reason,
                usage_delta,
            },
            Self::GenerationTransferred {
                fence,
                background_fence,
                task_id,
            } => OperationPatch::GenerationTransferred {
                fence,
                background_fence: background_fence.into_live(),
                task_id,
            },
            Self::Suspended {
                operation_id,
                cause,
            } => OperationPatch::Suspended {
                operation_id,
                cause,
            },
            Self::SuspensionRebasedAfterUnstartedResume {
                operation_id,
                previous_cause,
                replacement_fence,
                rebased_cause,
            } => OperationPatch::SuspensionRebasedAfterUnstartedResume {
                operation_id,
                previous_cause,
                replacement_fence,
                rebased_cause,
            },
            Self::RecoveryRequired {
                operation_id,
                last_generation,
            } => OperationPatch::RecoveryRequired {
                operation_id,
                last_generation,
            },
            Self::FinalizationStarted {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                selected_cause,
                suspended_cause,
                expected_settlements,
            } => OperationPatch::FinalizationStarted {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                selected_cause,
                suspended_cause,
                expected_settlements,
            },
            Self::FinalizationSettlementRecorded {
                operation_id,
                finalize_intent_id,
                receipt,
            } => OperationPatch::FinalizationSettlementRecorded {
                operation_id,
                finalize_intent_id,
                receipt,
            },
            Self::FinalizationDegraded {
                operation_id,
                finalize_intent_id,
                cause,
                last_error,
            } => OperationPatch::FinalizationDegraded {
                operation_id,
                finalize_intent_id,
                cause,
                last_error,
            },
            Self::Terminal { record } => OperationPatch::Terminal { record },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredTaskV1 {
    task_id: SurfaceTaskId,
    revision: TaskRevision,
    task_type: SurfaceTaskType,
    status: SurfaceTaskStatus,
    backgrounded: bool,
    description: DisplayText,
    created_at: UnixMillis,
    started_at: Option<UnixMillis>,
    completed_at: Option<UnixMillis>,
    parent_operation: Option<SurfaceOperationId>,
    parent_task_id: Option<SurfaceTaskId>,
    background_fence: Option<StoredBackgroundFenceV1>,
    workflow_run_id: Option<SurfaceWorkflowRunId>,
    subagent_id: Option<SurfaceSubagentId>,
    pending_interaction_id: Option<SurfaceInteractionId>,
    usage: Option<UsageTotals>,
    result: Option<DisplayText>,
    error: Option<DisplayText>,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    output_truncated: bool,
}

impl StoredTaskV1 {
    fn from_live(task: &SurfaceTask) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            task_id: task.task_id.clone(),
            revision: task.revision,
            task_type: task.task_type,
            status: task.status,
            backgrounded: task.backgrounded,
            description: task.description.clone(),
            created_at: task.created_at,
            started_at: task.started_at,
            completed_at: task.completed_at,
            parent_operation: task.parent_operation.clone(),
            parent_task_id: task.parent_task_id.clone(),
            background_fence: task
                .background_fence
                .as_ref()
                .map(StoredBackgroundFenceV1::from_live)
                .transpose()?,
            workflow_run_id: task.workflow_run_id.clone(),
            subagent_id: task.subagent_id.clone(),
            pending_interaction_id: task.pending_interaction_id.clone(),
            usage: task.usage.clone(),
            result: task.result.clone(),
            error: task.error.clone(),
            retry_count: task.retry_count,
            output_truncated: task.output_truncated,
        })
    }

    fn into_live(self) -> SurfaceTask {
        SurfaceTask {
            task_id: self.task_id,
            revision: self.revision,
            task_type: self.task_type,
            status: self.status,
            backgrounded: self.backgrounded,
            description: self.description,
            created_at: self.created_at,
            started_at: self.started_at,
            completed_at: self.completed_at,
            parent_operation: self.parent_operation,
            parent_task_id: self.parent_task_id,
            background_fence: self
                .background_fence
                .map(StoredBackgroundFenceV1::into_live),
            workflow_run_id: self.workflow_run_id,
            subagent_id: self.subagent_id,
            pending_interaction_id: self.pending_interaction_id,
            usage: self.usage,
            result: self.result,
            error: self.error,
            retry_count: self.retry_count,
            output_truncated: self.output_truncated,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredTaskPatchV1 {
    Upserted {
        expected_revision: Option<TaskRevision>,
        task: StoredTaskV1,
    },
    StatusChanged {
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        status: SurfaceTaskStatus,
        completed_at: Option<UnixMillis>,
        result: Option<DisplayText>,
        error: Option<DisplayText>,
    },
    InteractionChanged {
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        status: SurfaceTaskStatus,
        pending_interaction_id: Option<SurfaceInteractionId>,
    },
    OwnershipChanged {
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        backgrounded: bool,
        background_fence: Option<StoredBackgroundFenceV1>,
    },
    Reconciled {
        source_revision: TaskRevision,
        tasks: Vec<StoredTaskV1>,
    },
}

impl StoredTaskPatchV1 {
    fn from_live(patch: &TaskPatch) -> Result<Self, SurfaceLedgerError> {
        Ok(match patch {
            TaskPatch::Upserted {
                expected_revision,
                task,
            } => Self::Upserted {
                expected_revision: *expected_revision,
                task: StoredTaskV1::from_live(task)?,
            },
            TaskPatch::StatusChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                completed_at,
                result,
                error,
            } => Self::StatusChanged {
                task_id: task_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                status: *status,
                completed_at: *completed_at,
                result: result.clone(),
                error: error.clone(),
            },
            TaskPatch::InteractionChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                pending_interaction_id,
            } => Self::InteractionChanged {
                task_id: task_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                status: *status,
                pending_interaction_id: pending_interaction_id.clone(),
            },
            TaskPatch::OwnershipChanged {
                task_id,
                expected_revision,
                next_revision,
                backgrounded,
                background_fence,
            } => Self::OwnershipChanged {
                task_id: task_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                backgrounded: *backgrounded,
                background_fence: background_fence
                    .as_ref()
                    .map(StoredBackgroundFenceV1::from_live)
                    .transpose()?,
            },
            TaskPatch::Reconciled {
                source_revision,
                tasks,
            } => Self::Reconciled {
                source_revision: *source_revision,
                tasks: tasks
                    .iter()
                    .map(StoredTaskV1::from_live)
                    .collect::<Result<Vec<_>, _>>()?,
            },
        })
    }

    fn into_live(self) -> TaskPatch {
        match self {
            Self::Upserted {
                expected_revision,
                task,
            } => TaskPatch::Upserted {
                expected_revision,
                task: task.into_live(),
            },
            Self::StatusChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                completed_at,
                result,
                error,
            } => TaskPatch::StatusChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                completed_at,
                result,
                error,
            },
            Self::InteractionChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                pending_interaction_id,
            } => TaskPatch::InteractionChanged {
                task_id,
                expected_revision,
                next_revision,
                status,
                pending_interaction_id,
            },
            Self::OwnershipChanged {
                task_id,
                expected_revision,
                next_revision,
                backgrounded,
                background_fence,
            } => TaskPatch::OwnershipChanged {
                task_id,
                expected_revision,
                next_revision,
                backgrounded,
                background_fence: background_fence.map(StoredBackgroundFenceV1::into_live),
            },
            Self::Reconciled {
                source_revision,
                tasks,
            } => TaskPatch::Reconciled {
                source_revision,
                tasks: tasks.into_iter().map(StoredTaskV1::into_live).collect(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredAuthorityFingerprintV1 {
    operation_id: SurfaceOperationId,
    request_digest: Sha256Digest,
    tool_digest: Sha256Digest,
    cwd: CanonicalPath,
    workspace_roots_digest: Sha256Digest,
    policy_epoch: PolicyEpoch,
    executable_generation: Sha256Digest,
    artifact_generation: Sha256Digest,
    capability_digest: Sha256Digest,
}

impl StoredAuthorityFingerprintV1 {
    fn from_live(authority: &AuthorityFingerprint) -> Self {
        Self {
            operation_id: authority.operation_id().clone(),
            request_digest: authority.request_digest().clone(),
            tool_digest: authority.tool_digest().clone(),
            cwd: authority.cwd().clone(),
            workspace_roots_digest: authority.workspace_roots_digest().clone(),
            policy_epoch: authority.policy_epoch(),
            executable_generation: authority.executable_generation().clone(),
            artifact_generation: authority.artifact_generation().clone(),
            capability_digest: authority.capability_digest().clone(),
        }
    }

    fn into_live(self) -> AuthorityFingerprint {
        AuthorityFingerprint::new(
            self.operation_id,
            self.request_digest,
            self.tool_digest,
            self.cwd,
            self.workspace_roots_digest,
            self.policy_epoch,
            self.executable_generation,
            self.artifact_generation,
            self.capability_digest,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredTaskFenceV1 {
    task_id: SurfaceTaskId,
    task_revision: TaskRevision,
    background_owner: Option<StoredBackgroundFenceV1>,
}

impl StoredTaskFenceV1 {
    fn from_live(fence: &SurfaceTaskFence) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            task_id: fence.task_id.clone(),
            task_revision: fence.task_revision,
            background_owner: fence
                .background_owner
                .as_ref()
                .map(StoredBackgroundFenceV1::from_live)
                .transpose()?,
        })
    }

    fn into_live(self) -> SurfaceTaskFence {
        SurfaceTaskFence {
            task_id: self.task_id,
            task_revision: self.task_revision,
            background_owner: self
                .background_owner
                .map(StoredBackgroundFenceV1::into_live),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredInteractionRequestV1 {
    ToolApproval {
        tool: SurfaceToolRequest,
        description: DisplayText,
        preview: Option<DisplayText>,
        authority: StoredAuthorityFingerprintV1,
    },
    PermissionRequest {
        tool_call_id: SurfaceToolCallId,
        context: SurfacePermissionContext,
        reason: Option<DisplayText>,
        permissions: SurfacePermissionProfile,
        authority: StoredAuthorityFingerprintV1,
    },
    UserInput {
        question: NonEmptyText,
        suggestions: Vec<DisplayText>,
    },
    McpElicitation {
        server_name: NonEmptyText,
        server_request_id: NonEmptyText,
        message: DisplayText,
        request: SurfaceMcpElicitationRequest,
    },
    BackgroundApproval {
        task: StoredTaskFenceV1,
        tool: SurfaceToolRequest,
        authority: StoredAuthorityFingerprintV1,
    },
}

impl StoredInteractionRequestV1 {
    fn from_live(request: &SurfaceInteractionRequest) -> Result<Self, SurfaceLedgerError> {
        Ok(match request {
            SurfaceInteractionRequest::ToolApproval {
                tool,
                description,
                preview,
                authority,
            } => Self::ToolApproval {
                tool: tool.clone(),
                description: description.clone(),
                preview: preview.clone(),
                authority: StoredAuthorityFingerprintV1::from_live(authority),
            },
            SurfaceInteractionRequest::PermissionRequest {
                tool_call_id,
                context,
                reason,
                permissions,
                authority,
            } => Self::PermissionRequest {
                tool_call_id: tool_call_id.clone(),
                context: context.clone(),
                reason: reason.clone(),
                permissions: permissions.clone(),
                authority: StoredAuthorityFingerprintV1::from_live(authority),
            },
            SurfaceInteractionRequest::UserInput {
                question,
                suggestions,
            } => Self::UserInput {
                question: question.clone(),
                suggestions: suggestions.clone(),
            },
            SurfaceInteractionRequest::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            } => Self::McpElicitation {
                server_name: server_name.clone(),
                server_request_id: server_request_id.clone(),
                message: message.clone(),
                request: request.clone(),
            },
            SurfaceInteractionRequest::BackgroundApproval {
                task,
                tool,
                authority,
            } => Self::BackgroundApproval {
                task: StoredTaskFenceV1::from_live(task)?,
                tool: tool.clone(),
                authority: StoredAuthorityFingerprintV1::from_live(authority),
            },
        })
    }

    fn into_live(self) -> SurfaceInteractionRequest {
        match self {
            Self::ToolApproval {
                tool,
                description,
                preview,
                authority,
            } => SurfaceInteractionRequest::ToolApproval {
                tool,
                description,
                preview,
                authority: authority.into_live(),
            },
            Self::PermissionRequest {
                tool_call_id,
                context,
                reason,
                permissions,
                authority,
            } => SurfaceInteractionRequest::PermissionRequest {
                tool_call_id,
                context,
                reason,
                permissions,
                authority: authority.into_live(),
            },
            Self::UserInput {
                question,
                suggestions,
            } => SurfaceInteractionRequest::UserInput {
                question,
                suggestions,
            },
            Self::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            } => SurfaceInteractionRequest::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            },
            Self::BackgroundApproval {
                task,
                tool,
                authority,
            } => SurfaceInteractionRequest::BackgroundApproval {
                task: task.into_live(),
                tool,
                authority: authority.into_live(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredInteractionLifecycleV1 {
    Requested,
    Resolved {
        receipt: SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        reason: InteractionCancelReason,
    },
    Expired {
        deadline: InteractionExpiryDeadline,
    },
    Transferred {
        background_fence: StoredBackgroundFenceV1,
    },
}

impl StoredInteractionLifecycleV1 {
    fn from_live(lifecycle: &SurfaceInteractionLifecycle) -> Result<Self, SurfaceLedgerError> {
        Ok(match lifecycle {
            SurfaceInteractionLifecycle::Requested => Self::Requested,
            SurfaceInteractionLifecycle::Resolved { receipt } => Self::Resolved {
                receipt: receipt.clone(),
            },
            SurfaceInteractionLifecycle::Cancelled { reason } => Self::Cancelled {
                reason: reason.clone(),
            },
            SurfaceInteractionLifecycle::Expired { deadline } => Self::Expired {
                deadline: deadline.clone(),
            },
            SurfaceInteractionLifecycle::Transferred { background_fence } => Self::Transferred {
                background_fence: StoredBackgroundFenceV1::from_live(background_fence)?,
            },
        })
    }

    fn into_live(self) -> SurfaceInteractionLifecycle {
        match self {
            Self::Requested => SurfaceInteractionLifecycle::Requested,
            Self::Resolved { receipt } => SurfaceInteractionLifecycle::Resolved { receipt },
            Self::Cancelled { reason } => SurfaceInteractionLifecycle::Cancelled { reason },
            Self::Expired { deadline } => SurfaceInteractionLifecycle::Expired { deadline },
            Self::Transferred { background_fence } => SurfaceInteractionLifecycle::Transferred {
                background_fence: background_fence.into_live(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredInteractionViewV1 {
    interaction_id: SurfaceInteractionId,
    revision: InteractionRevision,
    fence: SurfaceOperationFence,
    kind: SurfaceInteractionKind,
    request: StoredInteractionRequestV1,
    route: SurfaceInteractionRoute,
    lifecycle: StoredInteractionLifecycleV1,
    recovery_disposition: InteractionUnavailableDisposition,
}

impl StoredInteractionViewV1 {
    fn from_live(interaction: &SurfaceInteractionView) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            interaction_id: interaction.interaction_id.clone(),
            revision: interaction.revision,
            fence: interaction.fence.clone(),
            kind: interaction.kind,
            request: StoredInteractionRequestV1::from_live(&interaction.request)?,
            route: interaction.route.clone(),
            lifecycle: StoredInteractionLifecycleV1::from_live(&interaction.lifecycle)?,
            recovery_disposition: interaction.recovery_disposition.clone(),
        })
    }

    fn into_live(self) -> SurfaceInteractionView {
        SurfaceInteractionView {
            interaction_id: self.interaction_id,
            revision: self.revision,
            fence: self.fence,
            kind: self.kind,
            request: self.request.into_live(),
            route: self.route,
            lifecycle: self.lifecycle.into_live(),
            recovery_disposition: self.recovery_disposition,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredInteractionPatchV1 {
    Requested {
        interaction: StoredInteractionViewV1,
    },
    RouteChanged {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        route: SurfaceInteractionRoute,
    },
    Resolved {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt: SurfaceInteractionResolutionReceipt,
        #[serde(default)]
        continuation: Option<DurableInteractionContinuationAnswer>,
    },
    ContinuationDispatchStarted {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: super::SurfaceResponseReceiptId,
        dispatch_id: SurfaceSettlementId,
        operation_id: SurfaceOperationId,
        turn_id: SurfaceTurnId,
    },
    ContinuationDispatchConsumed {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: super::SurfaceResponseReceiptId,
        dispatch_id: SurfaceSettlementId,
        operation_id: SurfaceOperationId,
        turn_id: SurfaceTurnId,
    },
    Cancelled {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        reason: InteractionCancelReason,
    },
    Expired {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        deadline: InteractionExpiryDeadline,
    },
    Transferred {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        background_fence: StoredBackgroundFenceV1,
        route: SurfaceInteractionRoute,
    },
}

impl StoredInteractionPatchV1 {
    fn from_live(patch: &InteractionPatch) -> Result<Self, SurfaceLedgerError> {
        Ok(match patch {
            InteractionPatch::Requested { interaction } => Self::Requested {
                interaction: StoredInteractionViewV1::from_live(interaction)?,
            },
            InteractionPatch::RouteChanged {
                interaction_id,
                expected_revision,
                next_revision,
                route,
            } => Self::RouteChanged {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                route: route.clone(),
            },
            InteractionPatch::Resolved {
                interaction_id,
                expected_revision,
                next_revision,
                receipt,
                continuation,
            } => Self::Resolved {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                receipt: receipt.clone(),
                continuation: continuation.clone(),
            },
            InteractionPatch::ContinuationDispatchStarted {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            } => Self::ContinuationDispatchStarted {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                receipt_id: receipt_id.clone(),
                dispatch_id: dispatch_id.clone(),
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
            },
            InteractionPatch::ContinuationDispatchConsumed {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            } => Self::ContinuationDispatchConsumed {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                receipt_id: receipt_id.clone(),
                dispatch_id: dispatch_id.clone(),
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
            },
            InteractionPatch::Cancelled {
                interaction_id,
                expected_revision,
                next_revision,
                reason,
            } => Self::Cancelled {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                reason: reason.clone(),
            },
            InteractionPatch::Expired {
                interaction_id,
                expected_revision,
                next_revision,
                deadline,
            } => Self::Expired {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                deadline: deadline.clone(),
            },
            InteractionPatch::Transferred {
                interaction_id,
                expected_revision,
                next_revision,
                background_fence,
                route,
            } => Self::Transferred {
                interaction_id: interaction_id.clone(),
                expected_revision: *expected_revision,
                next_revision: *next_revision,
                background_fence: StoredBackgroundFenceV1::from_live(background_fence)?,
                route: route.clone(),
            },
        })
    }

    fn into_live(self) -> InteractionPatch {
        match self {
            Self::Requested { interaction } => InteractionPatch::Requested {
                interaction: interaction.into_live(),
            },
            Self::RouteChanged {
                interaction_id,
                expected_revision,
                next_revision,
                route,
            } => InteractionPatch::RouteChanged {
                interaction_id,
                expected_revision,
                next_revision,
                route,
            },
            Self::Resolved {
                interaction_id,
                expected_revision,
                next_revision,
                receipt,
                continuation,
            } => InteractionPatch::Resolved {
                interaction_id,
                expected_revision,
                next_revision,
                receipt,
                continuation,
            },
            Self::ContinuationDispatchStarted {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            } => InteractionPatch::ContinuationDispatchStarted {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            },
            Self::ContinuationDispatchConsumed {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            } => InteractionPatch::ContinuationDispatchConsumed {
                interaction_id,
                expected_revision,
                next_revision,
                receipt_id,
                dispatch_id,
                operation_id,
                turn_id,
            },
            Self::Cancelled {
                interaction_id,
                expected_revision,
                next_revision,
                reason,
            } => InteractionPatch::Cancelled {
                interaction_id,
                expected_revision,
                next_revision,
                reason,
            },
            Self::Expired {
                interaction_id,
                expected_revision,
                next_revision,
                deadline,
            } => InteractionPatch::Expired {
                interaction_id,
                expected_revision,
                next_revision,
                deadline,
            },
            Self::Transferred {
                interaction_id,
                expected_revision,
                next_revision,
                background_fence,
                route,
            } => InteractionPatch::Transferred {
                interaction_id,
                expected_revision,
                next_revision,
                background_fence: background_fence.into_live(),
                route,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StoredSurfaceEventV1 {
    Operation(StoredOperationPatchV1),
    Item(ItemPatch),
    Assistant(AssistantPatch),
    Tool(ToolPatch),
    Plan(SurfacePlanSnapshot),
    Usage(SurfaceUsageSnapshot),
    Context(SurfaceContextSnapshot),
    Interaction(StoredInteractionPatchV1),
    Task(StoredTaskPatchV1),
    Workflow(WorkflowPatch),
    Subagent(SubagentPatch),
    Goal(GoalPatchEnvelope),
    Settings(SettingsPatch),
    McpCatalog(McpCatalogPatch),
    PinnedContext(PinnedContextPatch),
    Session(SessionPatch),
}

impl StoredSurfaceEventV1 {
    fn from_live(event: &SurfaceEvent) -> Result<Self, SurfaceLedgerError> {
        Ok(match event {
            SurfaceEvent::Operation(patch) => {
                Self::Operation(StoredOperationPatchV1::from_live(patch)?)
            }
            SurfaceEvent::Item(patch) => Self::Item(patch.clone()),
            SurfaceEvent::Assistant(patch) => Self::Assistant(patch.clone()),
            SurfaceEvent::Tool(patch) => Self::Tool(patch.clone()),
            SurfaceEvent::Plan(snapshot) => Self::Plan(snapshot.clone()),
            SurfaceEvent::Usage(snapshot) => Self::Usage(snapshot.clone()),
            SurfaceEvent::Context(snapshot) => Self::Context(snapshot.clone()),
            SurfaceEvent::Interaction(patch) => {
                Self::Interaction(StoredInteractionPatchV1::from_live(patch)?)
            }
            SurfaceEvent::Task(patch) => Self::Task(StoredTaskPatchV1::from_live(patch)?),
            SurfaceEvent::Workflow(patch) => Self::Workflow(patch.clone()),
            SurfaceEvent::Subagent(patch) => Self::Subagent(patch.clone()),
            SurfaceEvent::Goal(patch) => Self::Goal(patch.clone()),
            SurfaceEvent::Settings(patch) => Self::Settings(patch.clone()),
            SurfaceEvent::McpCatalog(patch) => Self::McpCatalog(patch.clone()),
            SurfaceEvent::PinnedContext(patch) => Self::PinnedContext(patch.clone()),
            SurfaceEvent::Session(patch) => Self::Session(patch.clone()),
        })
    }

    fn into_live(self) -> SurfaceEvent {
        match self {
            Self::Operation(patch) => SurfaceEvent::Operation(patch.into_live()),
            Self::Item(patch) => SurfaceEvent::Item(patch),
            Self::Assistant(patch) => SurfaceEvent::Assistant(patch),
            Self::Tool(patch) => SurfaceEvent::Tool(patch),
            Self::Plan(snapshot) => SurfaceEvent::Plan(snapshot),
            Self::Usage(snapshot) => SurfaceEvent::Usage(snapshot),
            Self::Context(snapshot) => SurfaceEvent::Context(snapshot),
            Self::Interaction(patch) => SurfaceEvent::Interaction(patch.into_live()),
            Self::Task(patch) => SurfaceEvent::Task(patch.into_live()),
            Self::Workflow(patch) => SurfaceEvent::Workflow(patch),
            Self::Subagent(patch) => SurfaceEvent::Subagent(patch),
            Self::Goal(patch) => SurfaceEvent::Goal(patch),
            Self::Settings(patch) => SurfaceEvent::Settings(patch),
            Self::McpCatalog(patch) => SurfaceEvent::McpCatalog(patch),
            Self::PinnedContext(patch) => SurfaceEvent::PinnedContext(patch),
            Self::Session(patch) => SurfaceEvent::Session(patch),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSurfaceEventEnvelopeV1 {
    ordinal: u32,
    event_id: SurfaceEventId,
    commit_class: CommitClass,
    scope: StoredScopeV1,
    event: StoredSurfaceEventV1,
}

impl StoredSurfaceEventEnvelopeV1 {
    fn from_live(envelope: &SurfaceEventEnvelope) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            ordinal: envelope.ordinal,
            event_id: envelope.event_id.clone(),
            commit_class: envelope.commit_class.clone(),
            scope: StoredScopeV1::from_live(&envelope.scope)?,
            event: StoredSurfaceEventV1::from_live(&envelope.event)?,
        })
    }

    fn into_live(self) -> SurfaceEventEnvelope {
        SurfaceEventEnvelope {
            ordinal: self.ordinal,
            event_id: self.event_id,
            commit_class: self.commit_class,
            scope: self.scope.into_live(),
            event: self.event.into_live(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredSurfaceCommitBatchV1 {
    version: u8,
    cursor_before: SurfaceCursor,
    cursor_after: SurfaceCursor,
    commit_class: CommitClass,
    event_count: u32,
    batch_digest: Sha256Digest,
    events: Vec<StoredSurfaceEventEnvelopeV1>,
}

impl StoredSurfaceCommitBatchV1 {
    pub(crate) fn from_live(batch: &SurfaceCommitBatch) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            version: 1,
            cursor_before: batch.cursor_before.clone(),
            cursor_after: batch.cursor_after.clone(),
            commit_class: batch.commit_class.clone(),
            event_count: batch.event_count,
            batch_digest: batch.batch_digest.clone(),
            events: batch
                .events
                .as_slice()
                .iter()
                .map(StoredSurfaceEventEnvelopeV1::from_live)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub(crate) fn into_live(self) -> Result<SurfaceCommitBatch, SurfaceLedgerError> {
        if self.version != 1 {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        let events = NonEmptyVec::try_new(
            self.events
                .into_iter()
                .map(StoredSurfaceEventEnvelopeV1::into_live)
                .collect(),
        )
        .map_err(|_| SurfaceLedgerError::AppendFailed)?;
        let batch = SurfaceCommitBatch {
            cursor_before: self.cursor_before,
            cursor_after: self.cursor_after,
            commit_class: self.commit_class,
            event_count: self.event_count,
            batch_digest: self.batch_digest,
            events,
        };
        match preflight_batch(&batch) {
            SurfaceCommitBatchPreflightResult::Ready {
                event_count,
                batch_digest,
                ..
            } if event_count == batch.event_count && batch_digest == batch.batch_digest => {
                Ok(batch)
            }
            _ => Err(SurfaceLedgerError::AppendFailed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredShutdownBarrierRecordV1 {
    version: u8,
    plan: StoredShutdownBarrierPlanV1,
    settled: Vec<StoredShutdownAckV1>,
    state: StoredShutdownBarrierStateV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownBarrierPlanV1 {
    CloseThread {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        thread: StoredShutdownThreadPlanV1,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        plan_digest: Sha256Digest,
    },
    ShutdownHost {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        threads: Vec<StoredShutdownThreadPlanV1>,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        final_host_lifecycle: StoredHostReceiptAckRequirementV1,
        plan_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownThreadPlanV1 {
    Recorded {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        operations: Vec<StoredShutdownOperationPlanV1>,
        session_closed: StoredThreadCursorAckRequirementV1,
        catalog_closed: StoredHostReceiptAckRequirementV1,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        persistence: StoredEphemeralThreadPersistenceV1,
        operations: Vec<StoredShutdownOperationPlanV1>,
        session_closed: StoredThreadCursorAckRequirementV1,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredEphemeralThreadPersistenceV1 {
    NonCataloguedOneShot {
        close_after: FirstOperationCompletionPolicy,
    },
    Attached,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownOperationPlanV1 {
    ExistingTerminal {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        requirement: StoredOperationTerminalAckRequirementV1,
    },
    PlannedFinalization {
        operation_id: SurfaceOperationId,
        source_phase: StoredShutdownOperationSourcePhaseV1,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        selected_cause: StoredShutdownSelectedCauseV1,
        expected_settlements: Vec<SurfaceSettlementId>,
        requirement: StoredOperationTerminalAckRequirementV1,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownOperationSourcePhaseV1 {
    Requested,
    AdmittedReserved,
    AdmittedStarted,
    Suspended,
    BackgroundOwned,
    Finalizing,
    FinalizingDegraded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownSelectedCauseV1 {
    ExistingWinning { cause: OperationFinalizationCause },
    Requested { host_shutdown: bool },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredThreadCursorAckRequirementV1 {
    thread_id: SurfaceThreadId,
    family: SurfaceFactFamily,
    event_id: SurfaceEventId,
    commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredOperationTerminalAckRequirementV1 {
    thread_id: SurfaceThreadId,
    thread_owner_epoch: ThreadOwnerEpoch,
    operation_id: SurfaceOperationId,
    terminal_commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredHostReceiptAckRequirementV1 {
    host_incarnation: HostIncarnation,
    identity: StoredShutdownHostRequirementIdentityV1,
    commit_id: SurfaceCommitId,
    receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownHostRequirementIdentityV1 {
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownBarrierStateV1 {
    Closing,
    Closed {
        retained_output: StoredRetainedShutdownOutputV1,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredRetainedShutdownOutputV1 {
    CloseThread {
        output: StoredClosedThreadReceiptV1,
    },
    ShutdownHost {
        host_incarnation: HostIncarnation,
        host_receipt: StoredHostShutdownReceiptV1,
        closed_threads: Vec<StoredClosedThreadReceiptV1>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredClosedThreadReceiptV1 {
    Recorded {
        thread_id: SurfaceThreadId,
        operation_terminals: Vec<StoredOperationTerminalAtCursorV1>,
        closed_cursor: SurfaceCursor,
        catalog_receipt: StoredSessionCatalogReceiptV1,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        persistence: StoredEphemeralThreadPersistenceV1,
        operation_terminals: Vec<StoredOperationTerminalAtCursorV1>,
        closed_cursor: SurfaceCursor,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredOperationTerminalAtCursorV1 {
    operation_id: SurfaceOperationId,
    terminal: OperationTerminal,
    #[serde(default)]
    completion_proof: super::SurfaceOperationCompletionProof,
    cursor: SurfaceCursor,
    commit_class: CommitClass,
    batch_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredSessionCatalogReceiptV1 {
    catalog_revision: SessionCatalogRevision,
    thread_id: Option<SurfaceThreadId>,
    action: StoredSessionCatalogActionV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredSessionCatalogActionV1 {
    Created,
    Opened,
    Loaded,
    Forked,
    Closed,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredHostShutdownReceiptV1 {
    host_incarnation: HostIncarnation,
    lifecycle_revision: HostLifecycleRevision,
    barrier_id: SurfaceSettlementId,
    shutdown_commit_id: SurfaceCommitId,
    closed_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredShutdownAckV1 {
    ThreadLocalCursor {
        cursor: SurfaceCursor,
        family: SurfaceFactFamily,
        event_id: SurfaceEventId,
        commit_class: CommitClass,
    },
    OperationTerminal {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        operation_id: SurfaceOperationId,
        value: StoredOperationTerminalAtCursorV1,
    },
    SessionCatalog {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
        receipt: StoredSessionCatalogReceiptV1,
        commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
        receipt: StoredHostShutdownReceiptV1,
        commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
}

impl StoredShutdownBarrierRecordV1 {
    pub(crate) fn from_live(record: &ShutdownBarrierRecord) -> Result<Self, SurfaceLedgerError> {
        Ok(Self {
            version: 1,
            plan: StoredShutdownBarrierPlanV1::from_live(&record.plan)?,
            settled: record
                .settled
                .iter()
                .map(StoredShutdownAckV1::from_live)
                .collect::<Result<Vec<_>, _>>()?,
            state: StoredShutdownBarrierStateV1::from_live(&record.state)?,
        })
    }

    pub(crate) fn into_live(self) -> Result<ShutdownBarrierRecord, SurfaceLedgerError> {
        if self.version != 1 {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        }
        let record = ShutdownBarrierRecord {
            plan: self.plan.into_live(),
            settled: self
                .settled
                .into_iter()
                .map(StoredShutdownAckV1::into_live)
                .collect(),
            state: self.state.into_live(),
        };
        ImmutableShutdownLedger::from_durable_record(record.clone())
            .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)?;
        Ok(record)
    }

    pub(crate) fn barrier_id(&self) -> &SurfaceSettlementId {
        match &self.plan {
            StoredShutdownBarrierPlanV1::CloseThread { barrier_id, .. }
            | StoredShutdownBarrierPlanV1::ShutdownHost { barrier_id, .. } => barrier_id,
        }
    }

    pub(crate) fn plan_digest(&self) -> &Sha256Digest {
        match &self.plan {
            StoredShutdownBarrierPlanV1::CloseThread { plan_digest, .. }
            | StoredShutdownBarrierPlanV1::ShutdownHost { plan_digest, .. } => plan_digest,
        }
    }
}

impl StoredShutdownBarrierPlanV1 {
    fn from_live(plan: &ShutdownBarrierPlan) -> Result<Self, SurfaceLedgerError> {
        Ok(match plan {
            ShutdownBarrierPlan::CloseThread {
                request_id,
                host_incarnation,
                thread,
                barrier_id,
                closing_commit_id,
                plan_digest,
            } => Self::CloseThread {
                request_id: request_id.clone(),
                host_incarnation: host_incarnation.clone(),
                thread: StoredShutdownThreadPlanV1::from_live(thread)?,
                barrier_id: barrier_id.clone(),
                closing_commit_id: closing_commit_id.clone(),
                plan_digest: plan_digest.clone(),
            },
            ShutdownBarrierPlan::ShutdownHost {
                request_id,
                host_incarnation,
                threads,
                barrier_id,
                closing_commit_id,
                final_host_lifecycle,
                plan_digest,
            } => Self::ShutdownHost {
                request_id: request_id.clone(),
                host_incarnation: host_incarnation.clone(),
                threads: threads
                    .iter()
                    .map(StoredShutdownThreadPlanV1::from_live)
                    .collect::<Result<Vec<_>, _>>()?,
                barrier_id: barrier_id.clone(),
                closing_commit_id: closing_commit_id.clone(),
                final_host_lifecycle: StoredHostReceiptAckRequirementV1::from_live(
                    final_host_lifecycle,
                )?,
                plan_digest: plan_digest.clone(),
            },
        })
    }

    fn into_live(self) -> ShutdownBarrierPlan {
        match self {
            Self::CloseThread {
                request_id,
                host_incarnation,
                thread,
                barrier_id,
                closing_commit_id,
                plan_digest,
            } => ShutdownBarrierPlan::CloseThread {
                request_id,
                host_incarnation,
                thread: thread.into_live(),
                barrier_id,
                closing_commit_id,
                plan_digest,
            },
            Self::ShutdownHost {
                request_id,
                host_incarnation,
                threads,
                barrier_id,
                closing_commit_id,
                final_host_lifecycle,
                plan_digest,
            } => ShutdownBarrierPlan::ShutdownHost {
                request_id,
                host_incarnation,
                threads: threads
                    .into_iter()
                    .map(StoredShutdownThreadPlanV1::into_live)
                    .collect(),
                barrier_id,
                closing_commit_id,
                final_host_lifecycle: final_host_lifecycle.into_live(),
                plan_digest,
            },
        }
    }
}

impl StoredShutdownThreadPlanV1 {
    fn from_live(plan: &ShutdownThreadPlan) -> Result<Self, SurfaceLedgerError> {
        Ok(match plan {
            ShutdownThreadPlan::Recorded {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                catalog_closed,
            } => Self::Recorded {
                thread_id: thread_id.clone(),
                owner_epoch: *owner_epoch,
                operations: operations
                    .iter()
                    .map(StoredShutdownOperationPlanV1::from_live)
                    .collect::<Result<Vec<_>, _>>()?,
                session_closed: session_closed.into(),
                catalog_closed: StoredHostReceiptAckRequirementV1::from_live(catalog_closed)?,
            },
            ShutdownThreadPlan::Ephemeral {
                thread_id,
                owner_epoch,
                persistence,
                operations,
                session_closed,
            } => Self::Ephemeral {
                thread_id: thread_id.clone(),
                owner_epoch: *owner_epoch,
                persistence: StoredEphemeralThreadPersistenceV1::from_live(persistence),
                operations: operations
                    .iter()
                    .map(StoredShutdownOperationPlanV1::from_live)
                    .collect::<Result<Vec<_>, _>>()?,
                session_closed: session_closed.into(),
            },
        })
    }

    fn into_live(self) -> ShutdownThreadPlan {
        match self {
            Self::Recorded {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                catalog_closed,
            } => ShutdownThreadPlan::Recorded {
                thread_id,
                owner_epoch,
                operations: operations
                    .into_iter()
                    .map(StoredShutdownOperationPlanV1::into_live)
                    .collect(),
                session_closed: session_closed.into(),
                catalog_closed: catalog_closed.into_live(),
            },
            Self::Ephemeral {
                thread_id,
                owner_epoch,
                persistence,
                operations,
                session_closed,
            } => ShutdownThreadPlan::Ephemeral {
                thread_id,
                owner_epoch,
                persistence: persistence.into_live(),
                operations: operations
                    .into_iter()
                    .map(StoredShutdownOperationPlanV1::into_live)
                    .collect(),
                session_closed: session_closed.into(),
            },
        }
    }
}

impl StoredEphemeralThreadPersistenceV1 {
    fn from_live(value: &EphemeralThreadPersistence) -> Self {
        match value {
            EphemeralThreadPersistence::EphemeralNonCataloguedOneShot { close_after } => {
                Self::NonCataloguedOneShot {
                    close_after: *close_after,
                }
            }
            EphemeralThreadPersistence::EphemeralAttached => Self::Attached,
        }
    }

    fn into_live(self) -> EphemeralThreadPersistence {
        match self {
            Self::NonCataloguedOneShot { close_after } => {
                EphemeralThreadPersistence::EphemeralNonCataloguedOneShot { close_after }
            }
            Self::Attached => EphemeralThreadPersistence::EphemeralAttached,
        }
    }
}

impl StoredShutdownOperationPlanV1 {
    fn from_live(value: &ShutdownOperationPlan) -> Result<Self, SurfaceLedgerError> {
        Ok(match value {
            ShutdownOperationPlan::ExistingTerminal {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                requirement,
            } => Self::ExistingTerminal {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                requirement: requirement.into(),
            },
            ShutdownOperationPlan::PlannedFinalization {
                operation_id,
                source_phase,
                finalize_intent_id,
                terminal_commit_id,
                selected_cause,
                expected_settlements,
                requirement,
            } => Self::PlannedFinalization {
                operation_id: operation_id.clone(),
                source_phase: (*source_phase).into(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: StoredShutdownSelectedCauseV1::from_live(selected_cause),
                expected_settlements: expected_settlements.clone(),
                requirement: requirement.into(),
            },
        })
    }

    fn into_live(self) -> ShutdownOperationPlan {
        match self {
            Self::ExistingTerminal {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                requirement,
            } => ShutdownOperationPlan::ExistingTerminal {
                operation_id,
                finalize_intent_id,
                terminal_commit_id,
                requirement: requirement.into(),
            },
            Self::PlannedFinalization {
                operation_id,
                source_phase,
                finalize_intent_id,
                terminal_commit_id,
                selected_cause,
                expected_settlements,
                requirement,
            } => ShutdownOperationPlan::PlannedFinalization {
                operation_id,
                source_phase: source_phase.into(),
                finalize_intent_id,
                terminal_commit_id,
                selected_cause: selected_cause.into_live(),
                expected_settlements,
                requirement: requirement.into(),
            },
        }
    }
}

impl From<ShutdownOperationSourcePhase> for StoredShutdownOperationSourcePhaseV1 {
    fn from(value: ShutdownOperationSourcePhase) -> Self {
        match value {
            ShutdownOperationSourcePhase::Requested => Self::Requested,
            ShutdownOperationSourcePhase::AdmittedReserved => Self::AdmittedReserved,
            ShutdownOperationSourcePhase::AdmittedStarted => Self::AdmittedStarted,
            ShutdownOperationSourcePhase::Suspended => Self::Suspended,
            ShutdownOperationSourcePhase::BackgroundOwned => Self::BackgroundOwned,
            ShutdownOperationSourcePhase::Finalizing => Self::Finalizing,
            ShutdownOperationSourcePhase::FinalizingDegraded => Self::FinalizingDegraded,
        }
    }
}

impl From<StoredShutdownOperationSourcePhaseV1> for ShutdownOperationSourcePhase {
    fn from(value: StoredShutdownOperationSourcePhaseV1) -> Self {
        match value {
            StoredShutdownOperationSourcePhaseV1::Requested => Self::Requested,
            StoredShutdownOperationSourcePhaseV1::AdmittedReserved => Self::AdmittedReserved,
            StoredShutdownOperationSourcePhaseV1::AdmittedStarted => Self::AdmittedStarted,
            StoredShutdownOperationSourcePhaseV1::Suspended => Self::Suspended,
            StoredShutdownOperationSourcePhaseV1::BackgroundOwned => Self::BackgroundOwned,
            StoredShutdownOperationSourcePhaseV1::Finalizing => Self::Finalizing,
            StoredShutdownOperationSourcePhaseV1::FinalizingDegraded => Self::FinalizingDegraded,
        }
    }
}

impl StoredShutdownSelectedCauseV1 {
    fn from_live(value: &ShutdownSelectedCause) -> Self {
        match value {
            ShutdownSelectedCause::ExistingWinning { cause } => Self::ExistingWinning {
                cause: cause.clone(),
            },
            ShutdownSelectedCause::Requested { cause } => Self::Requested {
                host_shutdown: *cause == ShutdownRequestCause::HostShutdown,
            },
        }
    }

    fn into_live(self) -> ShutdownSelectedCause {
        match self {
            Self::ExistingWinning { cause } => ShutdownSelectedCause::ExistingWinning { cause },
            Self::Requested { host_shutdown } => ShutdownSelectedCause::Requested {
                cause: if host_shutdown {
                    ShutdownRequestCause::HostShutdown
                } else {
                    ShutdownRequestCause::ThreadClose
                },
            },
        }
    }
}

impl From<&ThreadCursorAckRequirement> for StoredThreadCursorAckRequirementV1 {
    fn from(value: &ThreadCursorAckRequirement) -> Self {
        Self {
            thread_id: value.thread_id.clone(),
            family: value.family,
            event_id: value.event_id.clone(),
            commit_id: value.commit_id.clone(),
        }
    }
}

impl From<StoredThreadCursorAckRequirementV1> for ThreadCursorAckRequirement {
    fn from(value: StoredThreadCursorAckRequirementV1) -> Self {
        Self {
            thread_id: value.thread_id,
            family: value.family,
            event_id: value.event_id,
            commit_id: value.commit_id,
        }
    }
}

impl From<&OperationTerminalAckRequirement> for StoredOperationTerminalAckRequirementV1 {
    fn from(value: &OperationTerminalAckRequirement) -> Self {
        Self {
            thread_id: value.thread_id.clone(),
            thread_owner_epoch: value.thread_owner_epoch,
            operation_id: value.operation_id.clone(),
            terminal_commit_id: value.terminal_commit_id.clone(),
        }
    }
}

impl From<StoredOperationTerminalAckRequirementV1> for OperationTerminalAckRequirement {
    fn from(value: StoredOperationTerminalAckRequirementV1) -> Self {
        Self {
            thread_id: value.thread_id,
            thread_owner_epoch: value.thread_owner_epoch,
            operation_id: value.operation_id,
            terminal_commit_id: value.terminal_commit_id,
        }
    }
}

impl StoredHostReceiptAckRequirementV1 {
    fn from_live(value: &HostReceiptAckRequirement) -> Result<Self, SurfaceLedgerError> {
        let identity = match &value.identity {
            HostReceiptRequirementIdentity::SessionCatalog {
                thread_id,
                revision,
            } => StoredShutdownHostRequirementIdentityV1::SessionCatalog {
                thread_id: thread_id.clone(),
                revision: *revision,
            },
            HostReceiptRequirementIdentity::HostLifecycle {
                host_incarnation,
                revision,
            } => StoredShutdownHostRequirementIdentityV1::HostLifecycle {
                host_incarnation: host_incarnation.clone(),
                revision: *revision,
            },
            _ => return Err(SurfaceLedgerError::CommitIdentityConflict),
        };
        Ok(Self {
            host_incarnation: value.host_incarnation.clone(),
            identity,
            commit_id: value.commit_id.clone(),
            receipt_digest: value.receipt_digest.clone(),
        })
    }

    fn into_live(self) -> HostReceiptAckRequirement {
        let identity = match self.identity {
            StoredShutdownHostRequirementIdentityV1::SessionCatalog {
                thread_id,
                revision,
            } => HostReceiptRequirementIdentity::SessionCatalog {
                thread_id,
                revision,
            },
            StoredShutdownHostRequirementIdentityV1::HostLifecycle {
                host_incarnation,
                revision,
            } => HostReceiptRequirementIdentity::HostLifecycle {
                host_incarnation,
                revision,
            },
        };
        HostReceiptAckRequirement {
            host_incarnation: self.host_incarnation,
            identity,
            commit_id: self.commit_id,
            receipt_digest: self.receipt_digest,
        }
    }
}

impl StoredShutdownBarrierStateV1 {
    fn from_live(value: &ShutdownBarrierState) -> Result<Self, SurfaceLedgerError> {
        Ok(match value {
            ShutdownBarrierState::Closing => Self::Closing,
            ShutdownBarrierState::Closed { retained_output } => Self::Closed {
                retained_output: StoredRetainedShutdownOutputV1::from_live(retained_output)?,
            },
        })
    }

    fn into_live(self) -> ShutdownBarrierState {
        match self {
            Self::Closing => ShutdownBarrierState::Closing,
            Self::Closed { retained_output } => ShutdownBarrierState::Closed {
                retained_output: retained_output.into_live(),
            },
        }
    }
}

impl StoredRetainedShutdownOutputV1 {
    fn from_live(value: &RetainedShutdownOutput) -> Result<Self, SurfaceLedgerError> {
        Ok(match value {
            RetainedShutdownOutput::CloseThread { output } => Self::CloseThread {
                output: StoredClosedThreadReceiptV1::from_live(output),
            },
            RetainedShutdownOutput::ShutdownHost { output } => Self::ShutdownHost {
                host_incarnation: output.host_incarnation.clone(),
                host_receipt: StoredHostShutdownReceiptV1::from_live(&output.host_receipt),
                closed_threads: output
                    .closed_threads
                    .iter()
                    .map(StoredClosedThreadReceiptV1::from_live)
                    .collect(),
            },
        })
    }

    fn into_live(self) -> RetainedShutdownOutput {
        match self {
            Self::CloseThread { output } => RetainedShutdownOutput::CloseThread {
                output: output.into_live(),
            },
            Self::ShutdownHost {
                host_incarnation,
                host_receipt,
                closed_threads,
            } => RetainedShutdownOutput::ShutdownHost {
                output: ShutdownHostOutput {
                    host_incarnation,
                    host_receipt: host_receipt.into_live(),
                    closed_threads: closed_threads
                        .into_iter()
                        .map(StoredClosedThreadReceiptV1::into_live)
                        .collect(),
                },
            },
        }
    }
}

impl StoredClosedThreadReceiptV1 {
    fn from_live(value: &ClosedThreadReceipt) -> Self {
        match value {
            ClosedThreadReceipt::Recorded {
                thread_id,
                operation_terminals,
                closed_cursor,
                catalog_receipt,
            } => Self::Recorded {
                thread_id: thread_id.clone(),
                operation_terminals: operation_terminals
                    .iter()
                    .map(StoredOperationTerminalAtCursorV1::from_live)
                    .collect(),
                closed_cursor: closed_cursor.clone(),
                catalog_receipt: StoredSessionCatalogReceiptV1::from_live(catalog_receipt),
            },
            ClosedThreadReceipt::Ephemeral {
                thread_id,
                persistence,
                operation_terminals,
                closed_cursor,
            } => Self::Ephemeral {
                thread_id: thread_id.clone(),
                persistence: StoredEphemeralThreadPersistenceV1::from_live(persistence),
                operation_terminals: operation_terminals
                    .iter()
                    .map(StoredOperationTerminalAtCursorV1::from_live)
                    .collect(),
                closed_cursor: closed_cursor.clone(),
            },
        }
    }

    fn into_live(self) -> ClosedThreadReceipt {
        match self {
            Self::Recorded {
                thread_id,
                operation_terminals,
                closed_cursor,
                catalog_receipt,
            } => ClosedThreadReceipt::Recorded {
                thread_id,
                operation_terminals: operation_terminals
                    .into_iter()
                    .map(StoredOperationTerminalAtCursorV1::into_live)
                    .collect(),
                closed_cursor,
                catalog_receipt: catalog_receipt.into_live(),
            },
            Self::Ephemeral {
                thread_id,
                persistence,
                operation_terminals,
                closed_cursor,
            } => ClosedThreadReceipt::Ephemeral {
                thread_id,
                persistence: persistence.into_live(),
                operation_terminals: operation_terminals
                    .into_iter()
                    .map(StoredOperationTerminalAtCursorV1::into_live)
                    .collect(),
                closed_cursor,
            },
        }
    }
}

impl StoredOperationTerminalAtCursorV1 {
    fn from_live(value: &OperationTerminalAtCursor) -> Self {
        Self {
            operation_id: value.operation_id.clone(),
            terminal: value.terminal.clone(),
            completion_proof: value.completion_proof.clone(),
            cursor: value.cursor.clone(),
            commit_class: value.commit_class.clone(),
            batch_digest: value.batch_digest.clone(),
        }
    }

    fn into_live(self) -> OperationTerminalAtCursor {
        OperationTerminalAtCursor {
            operation_id: self.operation_id,
            terminal: self.terminal,
            completion_proof: self.completion_proof,
            cursor: self.cursor,
            commit_class: self.commit_class,
            batch_digest: self.batch_digest,
        }
    }
}

impl StoredSessionCatalogReceiptV1 {
    fn from_live(value: &SurfaceSessionCatalogReceipt) -> Self {
        Self {
            catalog_revision: value.catalog_revision,
            thread_id: value.thread_id.clone(),
            action: value.action.into(),
        }
    }

    fn into_live(self) -> SurfaceSessionCatalogReceipt {
        SurfaceSessionCatalogReceipt {
            catalog_revision: self.catalog_revision,
            thread_id: self.thread_id,
            action: self.action.into(),
        }
    }
}

impl From<SurfaceSessionCatalogAction> for StoredSessionCatalogActionV1 {
    fn from(value: SurfaceSessionCatalogAction) -> Self {
        match value {
            SurfaceSessionCatalogAction::Created => Self::Created,
            SurfaceSessionCatalogAction::Opened => Self::Opened,
            SurfaceSessionCatalogAction::Loaded => Self::Loaded,
            SurfaceSessionCatalogAction::Forked => Self::Forked,
            SurfaceSessionCatalogAction::Closed => Self::Closed,
            SurfaceSessionCatalogAction::Removed => Self::Removed,
        }
    }
}

impl From<StoredSessionCatalogActionV1> for SurfaceSessionCatalogAction {
    fn from(value: StoredSessionCatalogActionV1) -> Self {
        match value {
            StoredSessionCatalogActionV1::Created => Self::Created,
            StoredSessionCatalogActionV1::Opened => Self::Opened,
            StoredSessionCatalogActionV1::Loaded => Self::Loaded,
            StoredSessionCatalogActionV1::Forked => Self::Forked,
            StoredSessionCatalogActionV1::Closed => Self::Closed,
            StoredSessionCatalogActionV1::Removed => Self::Removed,
        }
    }
}

impl StoredHostShutdownReceiptV1 {
    fn from_live(value: &SurfaceHostShutdownReceipt) -> Self {
        Self {
            host_incarnation: value.host_incarnation.clone(),
            lifecycle_revision: value.lifecycle_revision,
            barrier_id: value.barrier_id.clone(),
            shutdown_commit_id: value.shutdown_commit_id.clone(),
            closed_at: value.closed_at,
        }
    }

    fn into_live(self) -> SurfaceHostShutdownReceipt {
        SurfaceHostShutdownReceipt {
            host_incarnation: self.host_incarnation,
            lifecycle_revision: self.lifecycle_revision,
            barrier_id: self.barrier_id,
            shutdown_commit_id: self.shutdown_commit_id,
            stage: SurfaceHostShutdownStage::Last,
            closed_at: self.closed_at,
        }
    }
}

impl StoredShutdownAckV1 {
    fn from_live(value: &MutationCommitAck) -> Result<Self, SurfaceLedgerError> {
        Ok(match value {
            MutationCommitAck::ThreadLocalCursor {
                cursor,
                family,
                event_id,
                commit_class,
            } => Self::ThreadLocalCursor {
                cursor: cursor.clone(),
                family: *family,
                event_id: event_id.clone(),
                commit_class: commit_class.clone(),
            },
            MutationCommitAck::OperationTerminalAck {
                thread_id,
                thread_owner_epoch,
                operation_id,
                value,
            } => Self::OperationTerminal {
                thread_id: thread_id.clone(),
                thread_owner_epoch: *thread_owner_epoch,
                operation_id: operation_id.clone(),
                value: StoredOperationTerminalAtCursorV1::from_live(value),
            },
            MutationCommitAck::HostCommitAck {
                host_incarnation,
                identity:
                    HostReceiptIdentityPair::SessionCatalog {
                        thread_id,
                        revision,
                        receipt,
                    },
                commit_id,
                receipt_digest,
            } => Self::SessionCatalog {
                host_incarnation: host_incarnation.clone(),
                thread_id: thread_id.clone(),
                revision: *revision,
                receipt: StoredSessionCatalogReceiptV1::from_live(receipt),
                commit_id: commit_id.clone(),
                receipt_digest: receipt_digest.clone(),
            },
            MutationCommitAck::HostCommitAck {
                host_incarnation,
                identity:
                    HostReceiptIdentityPair::HostLifecycle {
                        host_incarnation: identity_host,
                        revision,
                        receipt,
                    },
                commit_id,
                receipt_digest,
            } if identity_host == host_incarnation => Self::HostLifecycle {
                host_incarnation: host_incarnation.clone(),
                revision: *revision,
                receipt: StoredHostShutdownReceiptV1::from_live(receipt),
                commit_id: commit_id.clone(),
                receipt_digest: receipt_digest.clone(),
            },
            _ => return Err(SurfaceLedgerError::CommitIdentityConflict),
        })
    }

    fn into_live(self) -> MutationCommitAck {
        match self {
            Self::ThreadLocalCursor {
                cursor,
                family,
                event_id,
                commit_class,
            } => MutationCommitAck::ThreadLocalCursor {
                cursor,
                family,
                event_id,
                commit_class,
            },
            Self::OperationTerminal {
                thread_id,
                thread_owner_epoch,
                operation_id,
                value,
            } => MutationCommitAck::OperationTerminalAck {
                thread_id,
                thread_owner_epoch,
                operation_id,
                value: value.into_live(),
            },
            Self::SessionCatalog {
                host_incarnation,
                thread_id,
                revision,
                receipt,
                commit_id,
                receipt_digest,
            } => MutationCommitAck::HostCommitAck {
                host_incarnation,
                identity: HostReceiptIdentityPair::SessionCatalog {
                    thread_id,
                    revision,
                    receipt: receipt.into_live(),
                },
                commit_id,
                receipt_digest,
            },
            Self::HostLifecycle {
                host_incarnation,
                revision,
                receipt,
                commit_id,
                receipt_digest,
            } => MutationCommitAck::HostCommitAck {
                identity: HostReceiptIdentityPair::HostLifecycle {
                    host_incarnation: host_incarnation.clone(),
                    revision,
                    receipt: receipt.into_live(),
                },
                host_incarnation,
                commit_id,
                receipt_digest,
            },
        }
    }
}

pub struct JsonlSurfaceCommitLedger {
    path: PathBuf,
    cursor_template: SurfaceCursor,
    store: JsonlThreadStore,
    commit_index: Result<SurfaceCommitIndex, SurfaceLedgerError>,
}

#[derive(Clone)]
struct IndexedSurfaceCommit {
    committed: bool,
    batch: SurfaceCommitBatch,
}

#[derive(Clone, Default)]
struct SurfaceCommitIndex {
    ordered: Vec<IndexedSurfaceCommit>,
    by_id: BTreeMap<SurfaceCommitId, usize>,
}

impl SurfaceCommitIndex {
    fn commit_id(batch: &SurfaceCommitBatch) -> &SurfaceCommitId {
        match &batch.commit_class {
            CommitClass::Recorded { commit_id, .. } | CommitClass::Ephemeral { commit_id, .. } => {
                commit_id
            }
        }
    }

    fn from_stored(
        stored: Vec<(bool, StoredSurfaceCommitBatchV1)>,
    ) -> Result<Self, SurfaceLedgerError> {
        let mut index = Self::default();
        for (committed, stored_batch) in stored {
            let batch = stored_batch.into_live()?;
            let commit_id = Self::commit_id(&batch).clone();
            if index.by_id.contains_key(&commit_id) {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
            index.by_id.insert(commit_id, index.ordered.len());
            index
                .ordered
                .push(IndexedSurfaceCommit { committed, batch });
        }
        Ok(index)
    }

    fn get(&self, commit_id: &SurfaceCommitId) -> Option<&IndexedSurfaceCommit> {
        self.by_id
            .get(commit_id)
            .and_then(|index| self.ordered.get(*index))
    }

    fn get_mut(&mut self, commit_id: &SurfaceCommitId) -> Option<&mut IndexedSurfaceCommit> {
        let index = *self.by_id.get(commit_id)?;
        self.ordered.get_mut(index)
    }

    fn insert_prepared(&mut self, batch: SurfaceCommitBatch) {
        let commit_id = Self::commit_id(&batch).clone();
        self.by_id.insert(commit_id, self.ordered.len());
        self.ordered.push(IndexedSurfaceCommit {
            committed: false,
            batch,
        });
    }
}

#[cfg(test)]
static TERMINAL_APPEND_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static GOAL_CONTINUATION_APPEND_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static INTERACTION_ROUTE_APPEND_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static INTERACTION_REQUEST_APPEND_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static TERMINAL_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
#[cfg(test)]
static PENDING_TERMINAL_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static ADMISSION_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
#[cfg(test)]
static PENDING_ADMISSION_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PROVIDER_RESPONSE_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_PROVIDER_RESPONSE_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static SETTINGS_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
#[cfg(test)]
static PENDING_SETTINGS_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static TASK_OWNERSHIP_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_TASK_OWNERSHIP_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static BACKGROUND_TRANSFER_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_BACKGROUND_TRANSFER_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PROVIDER_COMPLETION_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_PROVIDER_COMPLETION_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES: OnceLock<
    Mutex<HashMap<PathBuf, usize>>,
> = OnceLock::new();
#[cfg(test)]
static CAPABILITY_DELIVERY_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PENDING_CAPABILITY_DELIVERY_CHECKPOINT_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PROVIDER_COMPLETION_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static PROVIDER_TERMINAL_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static TERMINAL_TASK_RECONCILIATION_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static ACTIVE_TASK_ADOPTION_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static GENERATION_APPEND_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static ADMISSION_REPAIR_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
#[cfg(test)]
static MANUAL_COMPACTION_COMPLETION_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();
#[cfg(test)]
static WORKFLOW_COMPLETION_APPEND_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();

pub struct JsonlSurfaceControlLedger {
    path: PathBuf,
    store: JsonlThreadStore,
}

impl JsonlSurfaceControlLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            store: JsonlThreadStore::new(),
        }
    }

    pub fn persist_owner_epoch(&self, owner_epoch: u64) -> Result<(), SurfaceLedgerError> {
        self.store
            .append_surface_owner_epoch(&self.path, owner_epoch)
            .map_err(|_| SurfaceLedgerError::AppendFailed)
    }

    pub fn persist_finalize_intent(
        &self,
        intent: &DurableFinalizeIntent,
    ) -> Result<(), SurfaceLedgerError> {
        if let Some(existing) = self
            .store
            .probe_surface_finalize_intent(&self.path, &intent.finalize_intent_id)
            .map_err(|_| SurfaceLedgerError::AppendFailed)?
        {
            return if existing == intent.expected_settlements {
                Ok(())
            } else {
                Err(SurfaceLedgerError::CommitIdentityConflict)
            };
        }
        self.store
            .append_surface_finalize_intent(
                &self.path,
                intent.finalize_intent_id.clone(),
                intent.expected_settlements.clone(),
            )
            .map_err(|_| SurfaceLedgerError::AppendFailed)
    }

    pub fn load_finalize_intent(
        &self,
        intent_id: &super::SurfaceFinalizeIntentId,
    ) -> Result<Option<DurableFinalizeIntent>, SurfaceLedgerError> {
        self.store
            .probe_surface_finalize_intent(&self.path, intent_id)
            .map_err(|_| SurfaceLedgerError::AppendFailed)?
            .map(|expected_settlements| {
                DurableFinalizeIntent::new(intent_id.clone(), expected_settlements)
                    .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)
            })
            .transpose()
    }

    pub fn persist_settlement(
        &self,
        receipt: &super::SurfaceSettlementReceipt,
    ) -> Result<(), SurfaceLedgerError> {
        self.store
            .append_surface_settlement(
                &self.path,
                canonical_id(&receipt.settlement_id),
                receipt.receipt_digest.as_bytes().to_vec(),
            )
            .map_err(|_| SurfaceLedgerError::AppendFailed)
    }

    pub fn persist_shutdown_barrier(
        &self,
        shutdown: &mut ImmutableShutdownLedger,
    ) -> Result<(), SurfaceLedgerError> {
        let record = shutdown
            .durable_record()
            .cloned()
            .ok_or(SurfaceLedgerError::CommitIdentityConflict)?;
        ImmutableShutdownLedger::from_durable_record(record.clone())
            .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)?;
        let stored = StoredShutdownBarrierRecordV1::from_live(&record)?;
        let id = canonical_id(stored.barrier_id());
        let plan_digest = stored.plan_digest().clone();
        if let Some((existing_digest, existing_stored)) = self
            .store
            .probe_surface_control_record(&self.path, "shutdown", &id)
            .map_err(|_| SurfaceLedgerError::AppendFailed)?
        {
            if existing_digest.as_slice() != plan_digest.as_bytes() {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
            let existing = existing_stored.into_live()?;
            if existing == record {
                shutdown
                    .mark_plan_durable(&record.plan)
                    .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)?;
                return Ok(());
            }
            let legal_progress = existing.plan == record.plan
                && matches!(existing.state, ShutdownBarrierState::Closing)
                && existing
                    .settled
                    .iter()
                    .all(|ack| record.settled.contains(ack));
            if !legal_progress {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
        }
        self.store
            .append_surface_shutdown_barrier(
                &self.path,
                id,
                plan_digest.as_bytes().to_vec(),
                stored,
            )
            .map_err(|_| SurfaceLedgerError::AppendFailed)?;
        shutdown
            .mark_plan_durable(&record.plan)
            .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)
    }

    pub fn load_shutdown_barrier(
        &self,
        barrier_id: &SurfaceSettlementId,
    ) -> Result<Option<ImmutableShutdownLedger>, SurfaceLedgerError> {
        let id = canonical_id(barrier_id);
        self.store
            .probe_surface_control_record(&self.path, "shutdown", &id)
            .map_err(|_| SurfaceLedgerError::AppendFailed)?
            .map(|(outer_digest, stored)| {
                if stored.barrier_id() != barrier_id
                    || outer_digest.as_slice() != stored.plan_digest().as_bytes()
                {
                    return Err(SurfaceLedgerError::CommitIdentityConflict);
                }
                ImmutableShutdownLedger::from_durable_record(stored.into_live()?)
                    .map_err(|_| SurfaceLedgerError::CommitIdentityConflict)
            })
            .transpose()
    }
}

fn canonical_id<T: serde::Serialize>(id: &T) -> String {
    serde_json::to_value(id)
        .expect("surface id serializes")
        .as_str()
        .expect("surface id is a string")
        .to_owned()
}

impl JsonlSurfaceCommitLedger {
    pub fn new(path: impl Into<PathBuf>, cursor_template: SurfaceCursor) -> Self {
        let path = path.into();
        let store = JsonlThreadStore::new();
        let commit_index = store
            .load_surface_commit_batches(&path)
            .map_err(Self::io_error)
            .and_then(SurfaceCommitIndex::from_stored);
        Self {
            path,
            cursor_template,
            store,
            commit_index,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn inject_terminal_append_failure_once(path: impl Into<PathBuf>) {
        TERMINAL_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_goal_continuation_append_failure_once(path: impl Into<PathBuf>) {
        GOAL_CONTINUATION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_interaction_route_append_failure_once(path: impl Into<PathBuf>) {
        INTERACTION_ROUTE_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_interaction_request_append_failure_once(path: impl Into<PathBuf>) {
        INTERACTION_REQUEST_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_terminal_checkpoint_failure_once(path: impl Into<PathBuf>) {
        Self::inject_terminal_checkpoint_failures(path, 1);
    }

    #[cfg(test)]
    pub(crate) fn inject_terminal_checkpoint_failures(path: impl Into<PathBuf>, count: usize) {
        TERMINAL_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn clear_terminal_checkpoint_failures(path: impl Into<PathBuf>) {
        let path = path.into();
        TERMINAL_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&path);
        PENDING_TERMINAL_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&path);
    }

    #[cfg(test)]
    pub(crate) fn inject_admission_checkpoint_failures(path: impl Into<PathBuf>, count: usize) {
        ADMISSION_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_provider_response_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        PROVIDER_RESPONSE_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_capability_delivery_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        CAPABILITY_DELIVERY_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_settings_checkpoint_failures(path: impl Into<PathBuf>, count: usize) {
        SETTINGS_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_task_ownership_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        TASK_OWNERSHIP_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn clear_task_ownership_checkpoint_failures(path: impl Into<PathBuf>) {
        let path = path.into();
        TASK_OWNERSHIP_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&path);
        PENDING_TASK_OWNERSHIP_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&path);
    }

    #[cfg(test)]
    pub(crate) fn inject_generation_append_failure_once(path: impl Into<PathBuf>) {
        GENERATION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_background_transfer_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        BACKGROUND_TRANSFER_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_provider_completion_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        PROVIDER_COMPLETION_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_background_approval_resume_checkpoint_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_provider_completion_append_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        PROVIDER_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_provider_terminal_append_failures(path: impl Into<PathBuf>, count: usize) {
        PROVIDER_TERMINAL_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_terminal_task_reconciliation_append_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        TERMINAL_TASK_RECONCILIATION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_active_task_adoption_append_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        ACTIVE_TASK_ADOPTION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_admission_repair_append_failure_once(path: impl Into<PathBuf>) {
        Self::inject_admission_repair_append_failures(path, 1);
    }

    #[cfg(test)]
    pub(crate) fn inject_admission_repair_append_failures(path: impl Into<PathBuf>, count: usize) {
        ADMISSION_REPAIR_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn clear_admission_repair_append_failures(path: impl Into<PathBuf>) {
        ADMISSION_REPAIR_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_manual_compaction_completion_append_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        MANUAL_COMPACTION_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    pub(crate) fn inject_workflow_completion_append_failures(
        path: impl Into<PathBuf>,
        count: usize,
    ) {
        WORKFLOW_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into(), count);
    }

    #[cfg(test)]
    fn take_terminal_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let is_terminal = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::Terminal { .. })
            )
        });
        is_terminal
            && TERMINAL_APPEND_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
    }

    #[cfg(test)]
    fn take_terminal_task_reconciliation_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let reconciles_terminal_tasks = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Task(TaskPatch::Reconciled { .. })
            )
        });
        if !reconciles_terminal_tasks {
            return false;
        }
        let mut failures = TERMINAL_TASK_RECONCILIATION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_active_task_adoption_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let events = batch.events.as_slice();
        let is_active_task_adoption = !events.is_empty()
            && events.len() % 5 == 0
            && events.chunks_exact(5).all(|group| {
                matches!(
                    (
                        &group[0].event,
                        &group[1].event,
                        &group[2].event,
                        &group[3].event,
                        &group[4].event,
                    ),
                    (
                        SurfaceEvent::Operation(OperationPatch::Requested { .. }),
                        SurfaceEvent::Operation(OperationPatch::Admitted { .. }),
                        SurfaceEvent::Operation(OperationPatch::GenerationStarted { .. }),
                        SurfaceEvent::Task(TaskPatch::Upserted {
                            task: SurfaceTask {
                                task_type: SurfaceTaskType::MainSession,
                                status: SurfaceTaskStatus::Running,
                                backgrounded: true,
                                ..
                            },
                            ..
                        }),
                        SurfaceEvent::Operation(OperationPatch::GenerationTransferred { .. }),
                    )
                )
            });
        if !is_active_task_adoption {
            return false;
        }
        let mut failures = ACTIVE_TASK_ADOPTION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_goal_continuation_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let is_goal_continuation = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Goal(super::GoalPatchEnvelope {
                    patch: super::GoalPatch::ContinuationDecided { .. },
                    ..
                })
            )
        });
        is_goal_continuation
            && GOAL_CONTINUATION_APPEND_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
    }

    #[cfg(test)]
    fn take_interaction_route_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let changes_route = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Interaction(InteractionPatch::RouteChanged { .. })
            )
        });
        changes_route
            && INTERACTION_ROUTE_APPEND_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
    }

    #[cfg(test)]
    fn take_interaction_request_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let requests_interaction = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Interaction(InteractionPatch::Requested { .. })
            )
        });
        requests_interaction
            && INTERACTION_REQUEST_APPEND_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
    }

    #[cfg(test)]
    fn take_generation_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let starts_generation = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::GenerationStarted { .. })
            )
        });
        starts_generation
            && GENERATION_APPEND_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
    }

    #[cfg(test)]
    fn take_admission_repair_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let repairs_admission = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(
                    OperationPatch::GenerationStopped { .. }
                        | OperationPatch::FinalizationStarted { .. }
                )
            )
        }) && batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::GenerationStopped { .. })
            )
        });
        if !repairs_admission {
            return false;
        }
        let mut failures = ADMISSION_REPAIR_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_terminal_checkpoint_failure(&self, batch: &SurfaceCommitBatch) {
        let is_terminal = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(
                    OperationPatch::Terminal { .. }
                        | OperationPatch::ControlIntentCommitted {
                            intent: super::PendingControlIntent::Terminalize { .. },
                            ..
                        }
                ) | SurfaceEvent::Goal(super::GoalPatchEnvelope {
                    patch: super::GoalPatch::OuterTurnFinished { .. },
                    ..
                })
            )
        });
        if !is_terminal {
            return;
        }
        let count = TERMINAL_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.path);
        if let Some(count) = count {
            PENDING_TERMINAL_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.path.clone())
                .and_modify(|pending| *pending += count)
                .or_insert(count);
        }
    }

    #[cfg(test)]
    fn take_pending_terminal_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_TERMINAL_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_manual_compaction_completion_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
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
        if !completes_manual_compaction {
            return false;
        }
        let mut failures = MANUAL_COMPACTION_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_workflow_completion_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let completes_workflow = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Workflow(
                    WorkflowPatch::Completed { .. }
                        | WorkflowPatch::Failed { .. }
                        | WorkflowPatch::Stopped { .. }
                        | WorkflowPatch::Cancelled { .. }
                )
            )
        });
        if !completes_workflow {
            return false;
        }
        let mut failures = WORKFLOW_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_admission_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_ADMISSION_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_background_transfer_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        let is_background_transfer = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::GenerationTransferred { .. })
            )
        });
        if !is_background_transfer {
            return;
        }
        let mut failures = BACKGROUND_TRANSFER_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_BACKGROUND_TRANSFER_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.path.clone(), count);
        }
    }

    #[cfg(test)]
    fn take_background_transfer_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_BACKGROUND_TRANSFER_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_provider_completion_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        let is_provider_completion = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                    reason: GenerationStopReason::ProviderSuspended
                        | GenerationStopReason::ExecutionFailed {
                            class: GenerationExecutionFailureClass::LegacyApprovalRequired,
                            ..
                        },
                    ..
                })
            )
        });
        if !is_provider_completion {
            return;
        }
        let mut failures = PROVIDER_COMPLETION_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_PROVIDER_COMPLETION_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.path.clone(), count);
        }
    }

    #[cfg(test)]
    fn take_provider_completion_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_PROVIDER_COMPLETION_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_background_approval_resume_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        let is_background_approval_resume = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::ControlIntentCommitted {
                    intent: PendingControlIntent::ResumeStarting { .. },
                    ..
                })
            )
        });
        if !is_background_approval_resume {
            return;
        }
        let mut failures = BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.path.clone(), count);
        }
    }

    #[cfg(test)]
    fn take_background_approval_resume_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_BACKGROUND_APPROVAL_RESUME_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_provider_completion_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let is_provider_completion = batch.events.as_slice().iter().any(|event| {
            matches!(
                (&event.scope, &event.event),
                (
                    SurfaceScope::Background { .. },
                    SurfaceEvent::Operation(OperationPatch::GenerationStopped { .. })
                )
            )
        }) && !batch
            .events
            .as_slice()
            .iter()
            .any(|event| matches!(event.event, SurfaceEvent::Workflow(_)));
        if !is_provider_completion {
            return false;
        }
        let mut failures = PROVIDER_COMPLETION_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_provider_terminal_append_failure(&self, batch: &SurfaceCommitBatch) -> bool {
        let is_provider_terminal = batch.events.as_slice().iter().any(|event| {
            matches!(
                (&event.scope, &event.event),
                (
                    SurfaceScope::Background { .. },
                    SurfaceEvent::Operation(OperationPatch::Terminal { .. })
                )
            )
        });
        if !is_provider_terminal {
            return false;
        }
        let mut failures = PROVIDER_TERMINAL_APPEND_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_provider_response_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        let is_provider_response = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { .. })
            )
        });
        if !is_provider_response {
            return;
        }
        let mut failures = PROVIDER_RESPONSE_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_PROVIDER_RESPONSE_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.path.clone())
                .and_modify(|pending| *pending += count)
                .or_insert(count);
        }
    }

    #[cfg(test)]
    fn arm_capability_delivery_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        let permits_delivery = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged {
                    call: SurfaceCapabilityCall {
                        kind: SurfaceCapabilityCallKind::WriteTextFile,
                        state: SurfaceCapabilityCallState::DeliveryPossible,
                        ..
                    },
                })
            )
        });
        if !permits_delivery {
            return;
        }
        let mut failures = CAPABILITY_DELIVERY_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_CAPABILITY_DELIVERY_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.path.clone(), count);
        }
    }

    #[cfg(test)]
    fn take_capability_delivery_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_CAPABILITY_DELIVERY_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn arm_settings_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        if !batch
            .events
            .as_slice()
            .iter()
            .any(|event| matches!(&event.event, SurfaceEvent::Settings(_)))
        {
            return;
        }
        let mut failures = SETTINGS_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_SETTINGS_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.path.clone())
                .and_modify(|pending| *pending += count)
                .or_insert(count);
        }
    }

    #[cfg(test)]
    fn arm_task_ownership_checkpoint_failures(&self, batch: &SurfaceCommitBatch) {
        if !batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Task(TaskPatch::OwnershipChanged { .. })
            )
        }) {
            return;
        }
        let mut failures = TASK_OWNERSHIP_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = failures.remove(&self.path) {
            PENDING_TASK_OWNERSHIP_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.path.clone())
                .and_modify(|pending| *pending += count)
                .or_insert(count);
        }
    }

    #[cfg(test)]
    fn take_task_ownership_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_TASK_OWNERSHIP_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_settings_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_SETTINGS_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    #[cfg(test)]
    fn take_provider_response_checkpoint_failure(&self) -> bool {
        let mut failures = PENDING_PROVIDER_RESPONSE_CHECKPOINT_FAILURES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = failures.get_mut(&self.path) else {
            return false;
        };
        if *count <= 1 {
            failures.remove(&self.path);
        } else {
            *count -= 1;
        }
        true
    }

    fn id_string(id: &SurfaceCommitId) -> String {
        serde_json::to_value(id)
            .expect("surface commit id serializes")
            .as_str()
            .expect("surface commit id is a string")
            .to_owned()
    }

    fn io_error(_: std::io::Error) -> SurfaceLedgerError {
        SurfaceLedgerError::AppendFailed
    }

    pub fn recover_batches(&self) -> Result<RecoveredSurfaceBatches, SurfaceLedgerError> {
        let index = self.commit_index.as_ref().map_err(Clone::clone)?;
        let mut committed = Vec::new();
        let mut prepared = None;
        let mut previous_cursor_after = None;
        for indexed in &index.ordered {
            let batch = indexed.batch.clone();
            if batch.cursor_before.thread_id != self.cursor_template.thread_id
                || previous_cursor_after.as_ref().map_or_else(
                    || batch.cursor_before.incarnation != self.cursor_template.incarnation,
                    |cursor| cursor != &batch.cursor_before,
                )
            {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
            previous_cursor_after = Some(batch.cursor_after.clone());
            if indexed.committed {
                if prepared.is_some() {
                    return Err(SurfaceLedgerError::CommitIdentityConflict);
                }
                committed.push(batch);
            } else if prepared.replace(batch).is_some() {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
        }
        Ok(RecoveredSurfaceBatches {
            committed,
            prepared,
        })
    }
}

impl SurfaceCommitLedger for JsonlSurfaceCommitLedger {
    fn append_complete_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError> {
        #[cfg(test)]
        if self.take_terminal_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_goal_continuation_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_interaction_route_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_interaction_request_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_generation_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_admission_repair_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_manual_compaction_completion_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_workflow_completion_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_provider_completion_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_provider_terminal_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_terminal_task_reconciliation_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        #[cfg(test)]
        if self.take_active_task_adoption_append_failure(batch) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        let (commit_id, durable_revision) = match &batch.commit_class {
            CommitClass::Recorded {
                commit_id,
                durable_revision,
                ..
            } => (commit_id.clone(), *durable_revision),
            CommitClass::Ephemeral { .. } => return Err(SurfaceLedgerError::AppendFailed),
        };
        let index = self.commit_index.as_mut().map_err(|error| error.clone())?;
        if let Some(existing) = index.get(&commit_id) {
            if existing.batch != *batch {
                return Err(SurfaceLedgerError::CommitIdentityConflict);
            }
            return Ok(SurfaceBatchReceipt::Recorded(DurableBatchReceipt {
                commit_id,
                durable_revision,
                event_count: batch.event_count,
                batch_digest: batch.batch_digest.clone(),
                cursor_after: batch.cursor_after.clone(),
            }));
        }
        let id = Self::id_string(&commit_id);
        self.store
            .append_surface_commit_prepared(
                &self.path,
                id,
                batch.event_count,
                batch.batch_digest.as_bytes().to_vec(),
                batch.cursor_before.next_seq.get(),
                batch.cursor_after.next_seq.get(),
                durable_revision.get(),
                StoredSurfaceCommitBatchV1::from_live(batch)?,
            )
            .map_err(Self::io_error)?;
        index.insert_prepared(batch.clone());
        let receipt = DurableBatchReceipt {
            commit_id,
            durable_revision,
            event_count: batch.event_count,
            batch_digest: batch.batch_digest.clone(),
            cursor_after: batch.cursor_after.clone(),
        };
        #[cfg(test)]
        self.arm_terminal_checkpoint_failure(batch);
        #[cfg(test)]
        self.arm_provider_response_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_capability_delivery_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_settings_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_task_ownership_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_background_transfer_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_provider_completion_checkpoint_failures(batch);
        #[cfg(test)]
        self.arm_background_approval_resume_checkpoint_failures(batch);
        #[cfg(test)]
        if batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(OperationPatch::Admitted { .. })
            )
        }) {
            let mut failures = ADMISSION_CHECKPOINT_FAILURES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(count) = failures.remove(&self.path) {
                PENDING_ADMISSION_CHECKPOINT_FAILURES
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .entry(self.path.clone())
                    .and_modify(|pending| *pending += count)
                    .or_insert(count);
            }
        }
        Ok(SurfaceBatchReceipt::Recorded(receipt))
    }

    fn checkpoint(&mut self, receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError> {
        let SurfaceBatchReceipt::Recorded(receipt) = receipt else {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        };
        #[cfg(test)]
        if self.take_pending_terminal_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_admission_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_provider_response_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_capability_delivery_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_settings_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_task_ownership_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_background_transfer_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_provider_completion_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        #[cfg(test)]
        if self.take_background_approval_resume_checkpoint_failure() {
            return Err(SurfaceLedgerError::CheckpointFailed);
        }
        self.store
            .append_surface_commit_committed(
                &self.path,
                Self::id_string(&receipt.commit_id),
                receipt.event_count,
                receipt.batch_digest.as_bytes().to_vec(),
                receipt.cursor_after.next_seq.get(),
                receipt.durable_revision.get(),
            )
            .map_err(|_| SurfaceLedgerError::CheckpointFailed)?;
        let index = self
            .commit_index
            .as_mut()
            .map_err(|_| SurfaceLedgerError::CheckpointFailed)?;
        let Some(indexed) = index.get_mut(&receipt.commit_id) else {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        };
        if indexed.batch.batch_digest != receipt.batch_digest
            || indexed.batch.event_count != receipt.event_count
            || indexed.batch.cursor_after != receipt.cursor_after
        {
            return Err(SurfaceLedgerError::CommitIdentityConflict);
        }
        indexed.committed = true;
        Ok(())
    }

    fn probe_commit(&self, commit_id: &SurfaceCommitId, digest: &Sha256Digest) -> CommitProbe {
        let Ok(index) = self.commit_index.as_ref() else {
            return CommitProbe::Absent;
        };
        let Some(indexed) = index.get(commit_id) else {
            return CommitProbe::Absent;
        };
        let batch = &indexed.batch;
        if &batch.batch_digest != digest {
            return CommitProbe::Conflict;
        }
        let CommitClass::Recorded {
            durable_revision, ..
        } = &batch.commit_class
        else {
            return CommitProbe::Conflict;
        };
        if indexed.committed {
            CommitProbe::Present(SurfaceBatchReceipt::Recorded(DurableBatchReceipt {
                commit_id: commit_id.clone(),
                durable_revision: *durable_revision,
                event_count: batch.event_count,
                batch_digest: digest.clone(),
                cursor_after: batch.cursor_after.clone(),
            }))
        } else {
            CommitProbe::Prepared(PreparedSurfaceCommit {
                commit_id: commit_id.clone(),
                event_count: batch.event_count,
                batch_digest: digest.clone(),
                cursor_before: batch.cursor_before.clone(),
                cursor_after: batch.cursor_after.clone(),
            })
        }
    }

    fn lookup_commit(&self, commit_id: &SurfaceCommitId) -> Option<SurfaceBatchReceipt> {
        let index = self.commit_index.as_ref().ok()?;
        let indexed = index.get(commit_id)?;
        if !indexed.committed {
            return None;
        }
        let CommitClass::Recorded {
            durable_revision, ..
        } = &indexed.batch.commit_class
        else {
            return None;
        };
        Some(SurfaceBatchReceipt::Recorded(DurableBatchReceipt {
            commit_id: commit_id.clone(),
            durable_revision: *durable_revision,
            event_count: indexed.batch.event_count,
            batch_digest: indexed.batch.batch_digest.clone(),
            cursor_after: indexed.batch.cursor_after.clone(),
        }))
    }

    fn lookup_subagent_source_digest(&self, commit_id: &SurfaceCommitId) -> Option<Sha256Digest> {
        let index = self.commit_index.as_ref().ok()?;
        let indexed = index.get(commit_id)?;
        indexed
            .committed
            .then(|| subagent_source_digest(&indexed.batch))?
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementError {
    EmptyIntent,
    DuplicateSettlement,
    StoreUnavailable,
    AmbiguousResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableFinalizeIntent {
    pub finalize_intent_id: super::SurfaceFinalizeIntentId,
    pub expected_settlements: Vec<super::SurfaceSettlementId>,
}

impl DurableFinalizeIntent {
    pub fn new(
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        expected_settlements: Vec<super::SurfaceSettlementId>,
    ) -> Result<Self, SettlementError> {
        if expected_settlements.is_empty() {
            return Err(SettlementError::EmptyIntent);
        }
        let unique = expected_settlements
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if unique.len() != expected_settlements.len() {
            return Err(SettlementError::DuplicateSettlement);
        }
        Ok(Self {
            finalize_intent_id,
            expected_settlements,
        })
    }
}

pub trait ExternalSettlementStore {
    fn probe(&self, id: &super::SurfaceSettlementId) -> Option<super::SurfaceSettlementReceipt>;

    fn apply_idempotent(
        &mut self,
        id: &super::SurfaceSettlementId,
    ) -> Result<super::SurfaceSettlementReceipt, SettlementError>;
}

pub fn reconcile_finalize_intent<S: ExternalSettlementStore + ?Sized>(
    intent: &DurableFinalizeIntent,
    store: &mut S,
) -> Result<Vec<super::SurfaceSettlementReceipt>, SettlementError> {
    let mut receipts = Vec::with_capacity(intent.expected_settlements.len());
    for settlement_id in &intent.expected_settlements {
        let receipt = match store.probe(settlement_id) {
            Some(receipt) => receipt,
            None => store.apply_idempotent(settlement_id)?,
        };
        if receipt.settlement_id != *settlement_id {
            return Err(SettlementError::AmbiguousResult);
        }
        receipts.push(receipt);
    }
    Ok(receipts)
}

pub trait InjectedRuntimeClock {
    fn clock_id(&self) -> super::HostMonotonicClockId;
    fn monotonic_tick(&self) -> u64;
    fn wall_clock_ms(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerLeaseKind {
    Thread,
    Policy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnerLeaseError {
    AlreadyOwned,
    DurableEpochUnavailable,
    EpochExhausted,
    IdentityMismatch,
}

enum OwnerLeaseBackend {
    Durable {
        _lock: ExclusiveFileLock,
        epoch_path: PathBuf,
    },
    ProcessLocalThread,
}

static PROCESS_LOCAL_THREAD_OWNERS: OnceLock<Mutex<BTreeSet<super::SurfaceThreadId>>> =
    OnceLock::new();

pub struct ExclusiveOwnerLease {
    backend: OwnerLeaseBackend,
    owner_epoch: u64,
    kind: OwnerLeaseKind,
    thread_id: Option<super::SurfaceThreadId>,
    diagnostic_clock_id: super::HostMonotonicClockId,
    diagnostic_tick: u64,
    diagnostic_wall_ms: i64,
}

impl ExclusiveOwnerLease {
    pub fn acquire(
        lock_path: impl Into<PathBuf>,
        epoch_path: impl Into<PathBuf>,
        kind: OwnerLeaseKind,
        clock: &impl InjectedRuntimeClock,
    ) -> Result<Self, OwnerLeaseError> {
        Self::acquire_bound(lock_path, epoch_path, kind, None, clock)
    }

    pub fn acquire_thread(
        lock_path: impl Into<PathBuf>,
        epoch_path: impl Into<PathBuf>,
        thread_id: super::SurfaceThreadId,
        clock: &impl InjectedRuntimeClock,
    ) -> Result<Self, OwnerLeaseError> {
        Self::acquire_bound(
            lock_path,
            epoch_path,
            OwnerLeaseKind::Thread,
            Some(thread_id),
            clock,
        )
    }

    pub fn acquire_process_local_thread(
        thread_id: super::SurfaceThreadId,
        clock: &impl InjectedRuntimeClock,
    ) -> Result<Self, OwnerLeaseError> {
        let mut owners = PROCESS_LOCAL_THREAD_OWNERS
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !owners.insert(thread_id.clone()) {
            return Err(OwnerLeaseError::AlreadyOwned);
        }
        drop(owners);
        Ok(Self {
            backend: OwnerLeaseBackend::ProcessLocalThread,
            owner_epoch: 1,
            kind: OwnerLeaseKind::Thread,
            thread_id: Some(thread_id),
            diagnostic_clock_id: clock.clock_id(),
            diagnostic_tick: clock.monotonic_tick(),
            diagnostic_wall_ms: clock.wall_clock_ms(),
        })
    }

    fn acquire_bound(
        lock_path: impl Into<PathBuf>,
        epoch_path: impl Into<PathBuf>,
        kind: OwnerLeaseKind,
        thread_id: Option<super::SurfaceThreadId>,
        clock: &impl InjectedRuntimeClock,
    ) -> Result<Self, OwnerLeaseError> {
        let lock_path = lock_path.into();
        let epoch_path = epoch_path.into();
        if !lease_paths_match(&lock_path, &epoch_path) {
            return Err(OwnerLeaseError::IdentityMismatch);
        }
        let lock = ExclusiveFileLock::try_acquire(&lock_path).map_err(|error| match error {
            PlatformError::LockContended { .. } => OwnerLeaseError::AlreadyOwned,
            _ => OwnerLeaseError::DurableEpochUnavailable,
        })?;
        let owner_epoch = advance_owner_epoch(&epoch_path)?;
        Ok(Self {
            backend: OwnerLeaseBackend::Durable {
                _lock: lock,
                epoch_path,
            },
            owner_epoch,
            kind,
            thread_id,
            diagnostic_clock_id: clock.clock_id(),
            diagnostic_tick: clock.monotonic_tick(),
            diagnostic_wall_ms: clock.wall_clock_ms(),
        })
    }

    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    pub fn kind(&self) -> OwnerLeaseKind {
        self.kind
    }

    pub fn has_authority(&self, _clock: &impl InjectedRuntimeClock) -> bool {
        self.has_current_authority()
    }

    pub(crate) fn has_current_authority(&self) -> bool {
        match &self.backend {
            OwnerLeaseBackend::Durable { epoch_path, .. } => {
                read_owner_epoch(epoch_path).is_ok_and(|epoch| epoch == self.owner_epoch)
            }
            OwnerLeaseBackend::ProcessLocalThread => {
                self.thread_id.as_ref().is_some_and(|thread_id| {
                    PROCESS_LOCAL_THREAD_OWNERS
                        .get_or_init(|| Mutex::new(BTreeSet::new()))
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .contains(thread_id)
                })
            }
        }
    }

    pub(crate) fn authorizes_thread(&self, thread_id: &super::SurfaceThreadId) -> bool {
        self.kind == OwnerLeaseKind::Thread
            && self.thread_id.as_ref() == Some(thread_id)
            && self.has_current_authority()
    }

    pub fn diagnostic_observation(&self) -> (&super::HostMonotonicClockId, u64, i64) {
        (
            &self.diagnostic_clock_id,
            self.diagnostic_tick,
            self.diagnostic_wall_ms,
        )
    }
}

impl Drop for ExclusiveOwnerLease {
    fn drop(&mut self) {
        match &self.backend {
            OwnerLeaseBackend::Durable { .. } => {}
            OwnerLeaseBackend::ProcessLocalThread => {
                if let Some(thread_id) = self.thread_id.as_ref() {
                    PROCESS_LOCAL_THREAD_OWNERS
                        .get_or_init(|| Mutex::new(BTreeSet::new()))
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(thread_id);
                }
            }
        }
    }
}

fn read_owner_epoch(path: &Path) -> Result<u64, OwnerLeaseError> {
    if !path.exists() {
        return Ok(0);
    }
    let mut value = String::new();
    File::open(path)
        .and_then(|mut file| file.read_to_string(&mut value))
        .map_err(|_| OwnerLeaseError::DurableEpochUnavailable)?;
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| OwnerLeaseError::DurableEpochUnavailable)
}

fn advance_owner_epoch(path: &Path) -> Result<u64, OwnerLeaseError> {
    let parent = path
        .parent()
        .ok_or(OwnerLeaseError::DurableEpochUnavailable)?;
    std::fs::create_dir_all(parent).map_err(|_| OwnerLeaseError::DurableEpochUnavailable)?;
    let next = read_owner_epoch(path)?
        .checked_add(1)
        .ok_or(OwnerLeaseError::EpochExhausted)?;
    atomic_write(
        path,
        format!("{next}\n").as_bytes(),
        AtomicWritePolicy::ReplaceDestination,
    )
    .map_err(|_| OwnerLeaseError::DurableEpochUnavailable)?;
    Ok(next)
}

fn lease_paths_match(lock_path: &Path, epoch_path: &Path) -> bool {
    lock_path.parent() == epoch_path.parent()
        && lock_path.file_stem().is_some()
        && lock_path.file_stem() == epoch_path.file_stem()
        && lock_path
            .extension()
            .is_some_and(|extension| extension == "lock")
        && epoch_path
            .extension()
            .is_some_and(|extension| extension == "epoch")
}
