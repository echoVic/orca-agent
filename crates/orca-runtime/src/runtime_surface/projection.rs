use super::identity::{
    ByteCount, ByteOffset, CanonicalMime, CanonicalPath, CanonicalUri, CapabilityRevision,
    ContextRevision, ContextWindowId, DisplayText, GoalCatalogRevision, GoalObjectiveRevision,
    GoalOwnerEpoch, GoalRevision, McpCatalogRevision, NonEmptyText, NonEmptyVec,
    PinnedContextRevision, PinnedContextSourceRevision, PlanRevision, PolicyEpoch,
    SafeDiagnosticText, SequenceNumber, SessionHealthRevision, SessionMetadataRevision,
    SettingsRevision, Sha256Digest, SubagentRevision, SurfaceBackgroundFence,
    SurfaceCapabilityCallId, SurfaceCatalogEntryId, SurfaceCommitId, SurfaceCursor,
    SurfaceFinalizeIntentId, SurfaceGenerationId, SurfaceGoalId, SurfaceGoalIntentId,
    SurfaceGoalOuterTurnId, SurfaceGoalRunId, SurfaceInputCorrelationId, SurfaceInteractionId,
    SurfaceItemId, SurfaceOperationFence, SurfaceOperationId, SurfaceRemoteTerminalId,
    SurfaceRequestId, SurfaceSettlementId, SurfaceStreamId, SurfaceSubagentId, SurfaceTaskId,
    SurfaceThreadId, SurfaceToolCallId, SurfaceTurnId, SurfaceValueError, SurfaceWorkflowFence,
    SurfaceWorkflowResultId, SurfaceWorkflowRunId, TaskRevision, ThreadOwnerEpoch,
    ToolInvocationRevision, UnixMillis, UsageRevision, UuidV7, WorkflowRevision,
};
use super::interaction::{
    DurableInteractionContinuationCapsule, DurableInteractionContinuationDisposition,
    DurableInteractionContinuationIntent, SurfaceSchema, SurfaceToolRequest,
    ToolInvocationCheckpoint,
};
use super::operation::{
    AdmittedInput, FailureClass, FinalizationDegradedCause, GenerationRecord,
    GenerationStartedWitness, GenerationStopReason, InputResolutionErrorCode, OperationBudget,
    OperationFinalizationCause, OperationRecord, OperationTerminal, OperationTerminalRecord,
    PendingControlIntent, SurfaceAgentLoopTurn, SurfaceGoalGenerationIdentity,
    SurfaceInputPresentation, SurfaceResolvedInputFact, SurfaceRuntimeSettings,
    SurfaceSettlementReceipt, SurfaceShutdownReason, SuspendedFinalizationCause, SuspensionCause,
    TerminalizationCause, UsageTotals,
};
use serde::{Deserialize, Deserializer, Serialize};

pub const ACP_CAPABILITY_TEXT_BYTE_LIMIT: usize = 4_194_304;
pub const ACP_CAPABILITY_IDENTIFIER_BYTE_LIMIT: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceVerificationResult {
    pub command: NonEmptyText,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: DisplayText,
    pub stderr: DisplayText,
}

#[derive(Clone, PartialEq)]
pub enum OperationPatch {
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
        background_fence: SurfaceBackgroundFence,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceItemOrigin {
    UserInput,
    GoalContinuation,
    WorkflowNotification,
    RuntimeContext,
    ProviderResponse,
    ToolResult,
    HistoryMaterialization,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceUserInputState {
    Pending {
        presentation: SurfaceInputPresentation,
        correlation_id: SurfaceInputCorrelationId,
    },
    Resolved {
        fact: SurfaceResolvedInputFact,
    },
    ResolutionFailed {
        presentation: SurfaceInputPresentation,
        correlation_id: SurfaceInputCorrelationId,
        code: InputResolutionErrorCode,
        message: SafeDiagnosticText,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAssistantMessageItem {
    pub id: SurfaceItemId,
    pub turn_id: SurfaceTurnId,
    pub text: DisplayText,
    pub pinned: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAssistantReasoningItem {
    pub id: SurfaceItemId,
    pub turn_id: SurfaceTurnId,
    pub summary: DisplayText,
    pub content: DisplayText,
    pub pinned: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAssistantPlanItem {
    pub id: SurfaceItemId,
    pub turn_id: SurfaceTurnId,
    pub text: DisplayText,
    pub pinned: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceItem {
    UserMessage {
        id: SurfaceItemId,
        turn_id: SurfaceTurnId,
        input: SurfaceUserInputState,
        pinned: bool,
        origin: SurfaceItemOrigin,
    },
    SystemMessage {
        id: SurfaceItemId,
        content: DisplayText,
        pinned: bool,
        origin: SurfaceItemOrigin,
    },
    AssistantMessage {
        id: SurfaceItemId,
        turn_id: SurfaceTurnId,
        text: DisplayText,
        pinned: bool,
    },
    AssistantReasoning {
        id: SurfaceItemId,
        turn_id: SurfaceTurnId,
        summary: DisplayText,
        content: DisplayText,
        pinned: bool,
    },
    AssistantPlan {
        id: SurfaceItemId,
        turn_id: SurfaceTurnId,
        text: DisplayText,
        pinned: bool,
    },
    ToolResultMessage {
        id: SurfaceItemId,
        turn_id: SurfaceTurnId,
        tool_call_id: SurfaceToolCallId,
        content: DisplayText,
        terminal: SurfaceToolTerminal,
        pinned: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ItemRemovalReason {
    Compacted,
    Backtracked,
    ForkExcluded,
    RecoveryRepair,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ItemPatch {
    Added {
        item: SurfaceItem,
    },
    InputResolved {
        item_id: SurfaceItemId,
        fact: SurfaceResolvedInputFact,
    },
    InputResolutionFailed {
        item_id: SurfaceItemId,
        code: InputResolutionErrorCode,
        message: SafeDiagnosticText,
    },
    Removed {
        item_id: SurfaceItemId,
        reason: ItemRemovalReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AssistantChannel {
    Message,
    Reasoning,
    Plan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceAssistantStreamState {
    Open,
    Completed,
    Discarded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAssistantStream {
    pub stream_id: SurfaceStreamId,
    pub fence: SurfaceOperationFence,
    pub turn_id: SurfaceTurnId,
    pub item_id: SurfaceItemId,
    pub channel: AssistantChannel,
    pub next_offset: ByteOffset,
    pub text: DisplayText,
    pub state: SurfaceAssistantStreamState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRawToolCall {
    pub id: SurfaceToolCallId,
    pub name: NonEmptyText,
    pub raw_arguments: DisplayText,
    pub arguments_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceCompletedModelResponse {
    pub response_id: UuidV7,
    pub turn_id: SurfaceTurnId,
    pub message_item: Option<SurfaceAssistantMessageItem>,
    pub reasoning_item: Option<SurfaceAssistantReasoningItem>,
    pub plan_item: Option<SurfaceAssistantPlanItem>,
    pub tool_calls: Vec<SurfaceRawToolCall>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AssistantDiscardReason {
    GenerationCancelled,
    GenerationInterrupted,
    ProviderFailed,
    RuntimeRestart,
    ProjectionRepair,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AssistantPatch {
    StreamOpened {
        stream: SurfaceAssistantStream,
    },
    Delta {
        stream_id: SurfaceStreamId,
        offset: ByteOffset,
        text: DisplayText,
    },
    ResponseCompleted {
        response: SurfaceCompletedModelResponse,
    },
    StreamDiscarded {
        stream_id: SurfaceStreamId,
        reason: AssistantDiscardReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceFileChange {
    UnifiedDiff {
        path: CanonicalPath,
        text: DisplayText,
        digest: Sha256Digest,
    },
    PreviewOmitted {
        path: CanonicalPath,
        input_bytes: ByteCount,
        maximum_bytes: ByteCount,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolInvocationStarted {
    Yes,
    No,
    Unknown,
}

/// Version 1 of the durable receipt that closes the pre-side-effect restart
/// window for one stable logical tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocationStartedReceiptV1 {
    invocation_id: SurfaceToolCallId,
    fence: SurfaceOperationFence,
    revision: ToolInvocationRevision,
}

impl ToolInvocationStartedReceiptV1 {
    pub(crate) fn new(
        invocation_id: SurfaceToolCallId,
        fence: SurfaceOperationFence,
        revision: ToolInvocationRevision,
    ) -> Self {
        Self {
            invocation_id,
            fence,
            revision,
        }
    }

    pub fn invocation_id(&self) -> &SurfaceToolCallId {
        &self.invocation_id
    }

    pub fn fence(&self) -> &SurfaceOperationFence {
        &self.fence
    }

    pub const fn revision(&self) -> ToolInvocationRevision {
        self.revision
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolInvocationRecoveryDisposition {
    RestartableBeforeInvocation,
    FailClosedStarted,
    FailClosedExecuting,
    FailClosedUnsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionRetryRecoveryDisposition {
    RestartablePreSideEffect,
    FailClosedExecuting,
    FailClosedUnsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolTerminalSource {
    Observed,
    CompatibilityRepair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceToolResultKind {
    Success,
    Failed,
    Denied,
    Cancelled,
    TimedOut,
    InvalidArguments,
    ExternalEffectAmbiguous,
    ObservationUnavailable,
    CleanupAmbiguous,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolTerminal {
    pub kind: SurfaceToolResultKind,
    pub source: ToolTerminalSource,
    pub invocation_started: ToolInvocationStarted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolResult {
    pub tool_call_id: SurfaceToolCallId,
    pub name: NonEmptyText,
    pub terminal: SurfaceToolTerminal,
    pub output: Option<DisplayText>,
    pub error: Option<DisplayText>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub file_change: Option<SurfaceFileChange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceToolViewState {
    Requested,
    Running,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolView {
    pub request: SurfaceToolRequest,
    pub state: SurfaceToolViewState,
    pub invocation_started: Option<ToolInvocationStartedReceiptV1>,
    pub arguments_bytes: ByteCount,
    pub output_bytes: ByteCount,
    pub streamed_output: DisplayText,
    pub streamed_output_truncated: bool,
    pub result: Option<SurfaceToolResult>,
    pub capability_calls: Vec<SurfaceCapabilityCall>,
    pub terminal_leases: Vec<SurfaceRemoteTerminalLease>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceCapabilityCallKind {
    ReadTextFile,
    WriteTextFile,
    TerminalCreate,
    TerminalOutput,
    TerminalWaitForExit,
    TerminalKill,
    TerminalRelease,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceTerminalExitStatus {
    pub exit_code: Option<u32>,
    pub signal: Option<NonEmptyText>,
}

macro_rules! bounded_capability_text {
    ($name:ident, $limit:ident, $allow_empty:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
                let value = value.into();
                if (!$allow_empty && value.is_empty()) || value.len() > $limit {
                    return Err(if value.is_empty() {
                        SurfaceValueError::Empty
                    } else {
                        SurfaceValueError::TooLong {
                            maximum: $limit,
                            observed: value.len(),
                        }
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::try_new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

bounded_capability_text!(AcpCapabilityText, ACP_CAPABILITY_TEXT_BYTE_LIMIT, true);
bounded_capability_text!(
    AcpCapabilityIdentifier,
    ACP_CAPABILITY_IDENTIFIER_BYTE_LIMIT,
    false
);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CapabilityCallResult {
    ReadTextFile {
        content: AcpCapabilityText,
        content_digest: Sha256Digest,
    },
    WriteTextFileAcknowledged,
    TerminalCreated {
        terminal_id: SurfaceRemoteTerminalId,
    },
    TerminalOutputObserved {
        output: AcpCapabilityText,
        truncated: bool,
        exit_status: Option<SurfaceTerminalExitStatus>,
    },
    TerminalExitObserved {
        exit_status: SurfaceTerminalExitStatus,
    },
    TerminalKillAcknowledged,
    TerminalReleaseAcknowledged,
    RemoteError {
        code: AcpCapabilityIdentifier,
        message: SafeDiagnosticText,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExternalEffectKind {
    FileWrite,
    TerminalCreate,
    TerminalKill,
    TerminalRelease,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceCapabilityCallState {
    Prepared,
    DeliveryPossible,
    WrittenAwaitingResponse,
    Completed {
        result: CapabilityCallResult,
        response_digest: Sha256Digest,
    },
    FailedBeforeWrite {
        error: SafeDiagnosticText,
    },
    ObservationUnavailable {
        error: SafeDiagnosticText,
    },
    ExternalEffectAmbiguous {
        effect_kind: ExternalEffectKind,
        error: SafeDiagnosticText,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceCapabilityCall {
    pub call_id: SurfaceCapabilityCallId,
    pub acp_session_id: NonEmptyText,
    pub fence: SurfaceOperationFence,
    pub capability_revision: CapabilityRevision,
    pub policy_epoch: PolicyEpoch,
    pub kind: SurfaceCapabilityCallKind,
    pub arguments_digest: Sha256Digest,
    pub owning_tool_call_id: SurfaceToolCallId,
    pub state: SurfaceCapabilityCallState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceRemoteTerminalLeaseState {
    Live {
        terminal_id: SurfaceRemoteTerminalId,
        owner_fence: SurfaceOperationFence,
    },
    KillPending {
        terminal_id: SurfaceRemoteTerminalId,
        owner_fence: SurfaceOperationFence,
    },
    ReleasePending {
        terminal_id: SurfaceRemoteTerminalId,
        owner_fence: SurfaceOperationFence,
    },
    Released,
    IdentityUnknown {
        create_call_id: SurfaceCapabilityCallId,
    },
    CleanupAmbiguous {
        terminal_id: Option<SurfaceRemoteTerminalId>,
        owner_fence: SurfaceOperationFence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRemoteTerminalLease {
    pub lease_id: UuidV7,
    pub owning_tool_call_id: SurfaceToolCallId,
    pub state: SurfaceRemoteTerminalLeaseState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolPatch {
    Requested {
        request: SurfaceToolRequest,
    },
    ArgumentsProgress {
        tool_call_id: SurfaceToolCallId,
        arguments_bytes: ByteCount,
    },
    OutputDelta {
        tool_call_id: SurfaceToolCallId,
        offset: ByteOffset,
        chunk: DisplayText,
    },
    InvocationStartedV1 {
        receipt: ToolInvocationStartedReceiptV1,
    },
    Completed {
        result: SurfaceToolResult,
    },
    CapabilityCallChanged {
        call: SurfaceCapabilityCall,
    },
    RemoteTerminalLeaseChanged {
        lease: SurfaceRemoteTerminalLease,
    },
}

impl super::commands::SurfaceSnapshot {
    /// Function intent contract:
    ///
    /// - Input: a recovered durable continuation capsule.
    /// - Output: `RestartableBeforeInvocation` only when the capsule, request,
    ///   checkpoint, fence, and projected tool agree and no start receipt or
    ///   later execution state exists; every other result is fail closed.
    /// - State changes and external calls: none; this is a read-only recovery
    ///   authorization check and never dispatches a tool.
    pub(crate) fn tool_invocation_recovery_disposition(
        &self,
        capsule: &DurableInteractionContinuationCapsule,
    ) -> ToolInvocationRecoveryDisposition {
        if capsule.disposition() == DurableInteractionContinuationDisposition::Executing {
            return ToolInvocationRecoveryDisposition::FailClosedExecuting;
        }
        if capsule.validate().is_err() {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        }
        let Some(DurableInteractionContinuationIntent::ToolInvocation(intent)) = capsule.intent()
        else {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        };
        if intent.checkpoint() != ToolInvocationCheckpoint::BeforeInvocation {
            return ToolInvocationRecoveryDisposition::FailClosedExecuting;
        }
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *intent.invocation_id())
        else {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        };
        if &tool.request != intent.request() {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        }
        let Some(projected_fence) = self
            .foreground_operation
            .iter()
            .chain(self.queued_operations.iter())
            .chain(self.operation_history.iter())
            .flat_map(|operation| operation.generations.iter())
            .filter(|generation| generation.logical_turn_id == tool.request.turn_id)
            .last()
            .map(|generation| &generation.fence)
        else {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        };
        if projected_fence != capsule.fence() {
            return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
        }
        if let Some(receipt) = &tool.invocation_started {
            if receipt.invocation_id() != intent.invocation_id()
                || receipt.fence() != projected_fence
            {
                return ToolInvocationRecoveryDisposition::FailClosedUnsafe;
            }
            return ToolInvocationRecoveryDisposition::FailClosedStarted;
        }
        if tool.state != SurfaceToolViewState::Requested {
            return ToolInvocationRecoveryDisposition::FailClosedExecuting;
        }
        ToolInvocationRecoveryDisposition::RestartableBeforeInvocation
    }

    /// Function intent contract:
    ///
    /// - Input: a recovered PermissionRetry capsule claiming a
    ///   `PreSideEffect` checkpoint.
    /// - Output: restart authority only when the capsule, request, stable tool
    ///   identity, operation fence, and projected not-started tool all agree.
    /// - Errors: represented as fail-closed dispositions; an executing,
    ///   completed, missing, or otherwise ambiguous tool is never restartable.
    /// - State changes and external calls: none; this check only reads the
    ///   durable projection and never grants permission or dispatches a tool.
    pub(crate) fn permission_retry_recovery_disposition(
        &self,
        capsule: &DurableInteractionContinuationCapsule,
    ) -> PermissionRetryRecoveryDisposition {
        if capsule.disposition() == DurableInteractionContinuationDisposition::Executing {
            return PermissionRetryRecoveryDisposition::FailClosedExecuting;
        }
        if capsule.validate().is_err() {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        }
        let Some(DurableInteractionContinuationIntent::PermissionRetry(intent)) = capsule.intent()
        else {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        };
        if intent.checkpoint() != super::PermissionRetryCheckpoint::PreSideEffect {
            return PermissionRetryRecoveryDisposition::FailClosedExecuting;
        }
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == *intent.invocation_id())
        else {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        };
        if &tool.request != intent.tool() {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        }
        if tool.invocation_started.is_some()
            || tool.state != SurfaceToolViewState::Requested
            || tool.result.is_some()
        {
            return PermissionRetryRecoveryDisposition::FailClosedExecuting;
        }
        let Some(projected_fence) = self
            .foreground_operation
            .iter()
            .chain(self.queued_operations.iter())
            .chain(self.operation_history.iter())
            .flat_map(|operation| operation.generations.iter())
            .filter(|generation| generation.logical_turn_id == tool.request.turn_id)
            .last()
            .map(|generation| &generation.fence)
        else {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        };
        if projected_fence != capsule.fence() {
            return PermissionRetryRecoveryDisposition::FailClosedUnsafe;
        }
        PermissionRetryRecoveryDisposition::RestartablePreSideEffect
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePlanPriority {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePlanItem {
    pub step: NonEmptyText,
    pub priority: SurfacePlanPriority,
    pub status: SurfacePlanStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePlanSnapshot {
    pub revision: PlanRevision,
    pub explanation: Option<DisplayText>,
    pub items: Vec<SurfacePlanItem>,
    pub causative_generation: Option<SurfaceOperationFence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceUsageSnapshot {
    pub revision: UsageRevision,
    pub thread_total: UsageTotals,
    pub active_operation: Option<(SurfaceOperationId, UsageTotals)>,
    pub goal: Option<(SurfaceGoalId, GoalUsage)>,
    pub workflow: Vec<(SurfaceWorkflowRunId, UsageTotals)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompactionReason {
    Manual,
    Automatic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompactionState {
    Idle,
    Running {
        operation_id: SurfaceOperationId,
        reason: CompactionReason,
        before_messages: u64,
    },
    Completed {
        operation_id: SurfaceOperationId,
        reason: CompactionReason,
        strategy: NonEmptyText,
        before_messages: u64,
        after_messages: u64,
        collapsed_messages: u64,
        status_text: DisplayText,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderReplayHealth {
    None,
    Available { state_digest: Sha256Digest },
    Invalidated { reason: DisplayText },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceContextFragmentKind {
    Runtime,
    Goal,
    Plan,
    Skill,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceContextFragmentOrigin {
    System,
    GoalRuntime,
    Model,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceContextFragment {
    pub id: NonEmptyText,
    pub kind: SurfaceContextFragmentKind,
    pub origin: SurfaceContextFragmentOrigin,
    pub content: DisplayText,
    pub max_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceContextSnapshot {
    pub revision: ContextRevision,
    #[serde(default)]
    pub window_id: ContextWindowId,
    pub used_tokens: u64,
    pub limit_tokens: u64,
    pub compaction: CompactionState,
    pub fragments: Vec<SurfaceContextFragment>,
    pub provider_replay: ProviderReplayHealth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSettingsSnapshot {
    pub host_revision: SettingsRevision,
    pub thread_revision: SettingsRevision,
    pub effective: SurfaceRuntimeSettings,
    pub pending: Option<SurfaceRuntimeSettings>,
    pub frozen_generation_revision: Option<(SurfaceOperationFence, SettingsRevision)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SettingsPatch {
    Committed {
        previous_revision: SettingsRevision,
        snapshot: SurfaceSettingsSnapshot,
    },
    PendingChanged {
        thread_revision: SettingsRevision,
        pending: Option<SurfaceRuntimeSettings>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceTaskType {
    MainSession,
    Workflow,
    Subagent,
    Shell,
    Monitor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceTaskStatus {
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

#[derive(Clone, PartialEq)]
pub struct SurfaceTask {
    pub task_id: SurfaceTaskId,
    pub revision: TaskRevision,
    pub task_type: SurfaceTaskType,
    pub status: SurfaceTaskStatus,
    pub backgrounded: bool,
    pub description: DisplayText,
    pub created_at: UnixMillis,
    pub started_at: Option<UnixMillis>,
    pub completed_at: Option<UnixMillis>,
    pub parent_operation: Option<SurfaceOperationId>,
    pub background_fence: Option<SurfaceBackgroundFence>,
    pub workflow_run_id: Option<SurfaceWorkflowRunId>,
    pub subagent_id: Option<SurfaceSubagentId>,
    pub pending_interaction_id: Option<SurfaceInteractionId>,
    pub usage: Option<UsageTotals>,
    pub result: Option<DisplayText>,
    pub error: Option<DisplayText>,
    pub retry_count: u32,
    pub output_truncated: bool,
}

#[derive(Clone, PartialEq)]
pub enum TaskPatch {
    Upserted {
        expected_revision: Option<TaskRevision>,
        task: SurfaceTask,
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
    OwnershipChanged {
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
        next_revision: TaskRevision,
        backgrounded: bool,
        background_fence: Option<SurfaceBackgroundFence>,
    },
    Reconciled {
        source_revision: TaskRevision,
        tasks: Vec<SurfaceTask>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceWorkflowStatus {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceWorkflowAgentStatus {
    Pending,
    Running,
    Cached,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflowPhase {
    pub name: NonEmptyText,
    pub status: SurfaceWorkflowStatus,
    pub started_at: Option<UnixMillis>,
    pub completed_at: Option<UnixMillis>,
    pub agent_count: u32,
    pub summary: Option<DisplayText>,
    pub error: Option<DisplayText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflowAgent {
    pub agent_id: SurfaceSubagentId,
    pub phase: NonEmptyText,
    pub status: SurfaceWorkflowAgentStatus,
    pub attempt: u32,
    pub output: Option<DisplayText>,
    pub error: Option<DisplayText>,
    pub usage: Option<UsageTotals>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceWorkflowResultStatus {
    Success,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflowResult {
    pub result_id: SurfaceWorkflowResultId,
    pub tool_use_id: Option<SurfaceToolCallId>,
    pub status: SurfaceWorkflowResultStatus,
    pub content: DisplayText,
    pub acknowledged_by_operation: Option<SurfaceOperationId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceWorkflow {
    pub workflow_run_id: SurfaceWorkflowRunId,
    pub task_id: SurfaceTaskId,
    pub revision: WorkflowRevision,
    pub name: NonEmptyText,
    pub status: SurfaceWorkflowStatus,
    pub phases: Vec<SurfaceWorkflowPhase>,
    pub agents: Vec<SurfaceWorkflowAgent>,
    pub result: Option<SurfaceWorkflowResult>,
    pub error: Option<DisplayText>,
    pub parent: Option<SurfaceOperationFence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkflowPatch {
    Started {
        workflow: SurfaceWorkflow,
    },
    Resumed {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
    },
    PhaseStarted {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        phase: SurfaceWorkflowPhase,
    },
    PhaseCompleted {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        phase: SurfaceWorkflowPhase,
    },
    AgentStarted {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        agent: SurfaceWorkflowAgent,
    },
    AgentCached {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        agent: SurfaceWorkflowAgent,
    },
    AgentCompleted {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        agent: SurfaceWorkflowAgent,
    },
    AgentFailed {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        agent: SurfaceWorkflowAgent,
    },
    AgentCancelled {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        agent: SurfaceWorkflowAgent,
    },
    Paused {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        reason: DisplayText,
    },
    Stopping {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        reason: DisplayText,
    },
    Stopped {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        reason: DisplayText,
    },
    AsyncLaunched {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
    },
    Completed {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
    },
    Failed {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        error: DisplayText,
    },
    Cancelled {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        reason: DisplayText,
    },
    ResultReady {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        result: SurfaceWorkflowResult,
    },
    ResultAcknowledged {
        fence: SurfaceWorkflowFence,
        next_revision: WorkflowRevision,
        result_id: SurfaceWorkflowResultId,
        operation_id: SurfaceOperationId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSubagent {
    pub subagent_id: SurfaceSubagentId,
    pub revision: SubagentRevision,
    pub description: DisplayText,
    pub status: SurfaceSubagentStatus,
    pub activity: Option<DisplayText>,
    pub turn: Option<u32>,
    pub usage: Option<UsageTotals>,
    pub output: Option<DisplayText>,
    pub error: Option<DisplayText>,
    pub parent: SurfaceOperationFence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RunningSurfaceSubagent(SurfaceSubagent);

impl RunningSurfaceSubagent {
    pub fn try_new(subagent: SurfaceSubagent) -> Result<Self, SurfaceValueError> {
        if subagent.status != SurfaceSubagentStatus::Running {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(subagent))
    }

    pub fn as_subagent(&self) -> &SurfaceSubagent {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunningSurfaceSubagent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(SurfaceSubagent::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSubagentTerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExpectedAbsentSubagentRevision;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SubagentPatch {
    Started {
        expected_revision: ExpectedAbsentSubagentRevision,
        subagent: RunningSurfaceSubagent,
    },
    Progress {
        subagent_id: SurfaceSubagentId,
        expected_revision: SubagentRevision,
        next_revision: SubagentRevision,
        parent: SurfaceOperationFence,
        activity: DisplayText,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    },
    Completed {
        subagent_id: SurfaceSubagentId,
        expected_revision: SubagentRevision,
        next_revision: SubagentRevision,
        parent: SurfaceOperationFence,
        status: SurfaceSubagentTerminalStatus,
        output: Option<DisplayText>,
        error: Option<DisplayText>,
        usage: Option<UsageTotals>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceEvidenceKind {
    Test,
    File,
    Command,
    Observation,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceEvidenceItem {
    pub kind: SurfaceEvidenceKind,
    pub summary: NonEmptyText,
    pub target: Option<DisplayText>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceBlockerKind {
    UserDecision,
    MissingAuthority,
    ExternalState,
    EnvironmentContradiction,
    UnverifiableRequirement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceBlocker {
    pub kind: SurfaceBlockerKind,
    pub summary: NonEmptyText,
    pub fingerprint: NonEmptyText,
    pub evidence: Vec<SurfaceEvidenceItem>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalPauseReason {
    User,
    NoProgress,
    Backoff,
    Infrastructure,
    WaitingForWorkflow,
    Recovery,
    UsageLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalState {
    Active,
    Paused {
        reason: SurfaceGoalPauseReason,
        message: DisplayText,
    },
    Blocked {
        blocker: SurfaceBlocker,
    },
    BudgetLimited,
    Complete {
        evidence: Vec<SurfaceEvidenceItem>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalUsage {
    pub charged_input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    pub verifier_tokens: i64,
    pub cost_micros: i64,
    pub elapsed_seconds: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalRunOrigin {
    User,
    Resume,
    WorkflowNotification,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalOuterTurnReceiptOrigin {
    User,
    Resume,
    Continuation,
    WorkflowNotification,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalOuterTurnReceipt {
    pub outer_turn_id: SurfaceGoalOuterTurnId,
    pub origin: SurfaceGoalOuterTurnReceiptOrigin,
    pub outer_turn_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalRunPhase {
    Preparing,
    InFlight {
        outer_turn: SurfaceGoalOuterTurnReceipt,
    },
    Settled {
        last_outer_turn: Option<SurfaceGoalOuterTurnReceipt>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalRun {
    pub goal_run_id: SurfaceGoalRunId,
    pub run_origin: SurfaceGoalRunOrigin,
    pub operation_id: SurfaceOperationId,
    pub phase: SurfaceGoalRunPhase,
}

pub type SurfaceGoalRunReceipt = SurfaceGoalRun;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalNoLiveRun {
    NoCurrentRun,
    Quiescent { run: SurfaceGoalRun },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalTransition {
    pub previous: SurfaceGoalState,
    pub next: SurfaceGoalState,
    pub reason_code: NonEmptyText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoal {
    pub goal_id: SurfaceGoalId,
    pub thread_id: SurfaceThreadId,
    pub goal_revision: GoalRevision,
    pub goal_owner_epoch: GoalOwnerEpoch,
    pub catalog_revision: GoalCatalogRevision,
    pub receipt_digest: Sha256Digest,
    pub objective: NonEmptyText,
    pub objective_revision: GoalObjectiveRevision,
    pub state: SurfaceGoalState,
    pub token_budget: Option<i64>,
    pub usage: GoalUsage,
    pub current_run: Option<SurfaceGoalRun>,
    pub last_transition: Option<SurfaceGoalTransition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalReceiptState {
    Present {
        state: SurfaceGoalState,
        current_run: Option<SurfaceGoalRunReceipt>,
    },
    Removed {
        tombstone_revision: GoalRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalStoreReceipt {
    pub goal_id: SurfaceGoalId,
    pub goal_revision: GoalRevision,
    pub objective_revision: GoalObjectiveRevision,
    pub catalog_revision: GoalCatalogRevision,
    pub goal_owner_epoch: GoalOwnerEpoch,
    pub row_state: SurfaceGoalReceiptState,
    pub store_commit_id: SurfaceCommitId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalCloseReason {
    Recovery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalClosedRunReceipt {
    pub run: SurfaceGoalRun,
    pub close_reason: SurfaceGoalCloseReason,
    pub store_commit_id: SurfaceCommitId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalIntent {
    Complete {
        intent_id: SurfaceGoalIntentId,
        reason: NonEmptyText,
        evidence: NonEmptyVec<SurfaceEvidenceItem>,
    },
    Blocked {
        intent_id: SurfaceGoalIntentId,
        reason: NonEmptyText,
        blocker: SurfaceBlocker,
        evidence: Vec<SurfaceEvidenceItem>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalIntentRejectionCode {
    NoActiveOuterTurn,
    TerminalIntentPending,
    MissingEvidence,
    MissingBlocker,
    StaleIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalIntentAck {
    DeferredToTurnEnd {
        intent_id: SurfaceGoalIntentId,
        pending_depth: u32,
    },
    Rejected {
        code: GoalIntentRejectionCode,
        message: DisplayText,
    },
    AlreadyPending {
        intent_id: SurfaceGoalIntentId,
    },
    BlockedAgainstInactive {
        state: SurfaceGoalState,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalPredecessorStatus {
    Failed,
    Cancelled,
    ApprovalRequired,
    BudgetExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GoalTokenBudget(OperationBudget);

impl GoalTokenBudget {
    pub fn try_new(budget: OperationBudget) -> Result<Self, SurfaceValueError> {
        if !matches!(budget, OperationBudget::GoalTokenBudget { .. }) {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(budget))
    }

    pub fn as_budget(&self) -> &OperationBudget {
        &self.0
    }
}

impl<'de> Deserialize<'de> for GoalTokenBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(OperationBudget::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalContinuationStopReason {
    GoalInactive {
        state: SurfaceGoalState,
    },
    PredecessorNotSuccessful {
        status: GoalPredecessorStatus,
        terminal: OperationTerminal,
    },
    TerminalizingControl {
        cause: TerminalizationCause,
    },
    QueuedUserInput {
        item_id: SurfaceItemId,
    },
    PendingInteraction {
        interaction_id: SurfaceInteractionId,
    },
    WorkflowOwned {
        workflow_run_id: SurfaceWorkflowRunId,
    },
    PlanModeDisallowsContinuation,
    VerificationPending,
    BudgetLimited {
        budget: GoalTokenBudget,
    },
    RuntimeFailure {
        class: FailureClass,
        message: SafeDiagnosticText,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalContinuationAdmitReason {
    Progress,
    GapFeedback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalContinuationDecision {
    Admitted {
        reason: GoalContinuationAdmitReason,
        successor: SurfaceGoalGenerationIdentity,
    },
    Stopped {
        reason: GoalContinuationStopReason,
        outer_turn_count: u32,
        goal_state: SurfaceGoalState,
        terminal: OperationTerminal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceGoalVerification {
    Achieved { evidence: Vec<SurfaceEvidenceItem> },
    NotAchieved { gaps: Vec<SurfaceGoalGap> },
    Blocked { blocker: SurfaceBlocker },
    Indeterminate { message: DisplayText },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalGap {
    pub summary: NonEmptyText,
    pub fingerprint: NonEmptyText,
    pub model_fixable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalOuterTurnStatus {
    Success,
    Failed,
    Cancelled,
    ApprovalRequired,
    BudgetExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalOuterTurnNextAction {
    Continue,
    Verify,
    Pause,
    Blocked,
    BudgetLimited,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DiscardedContinuation(bool);

impl DiscardedContinuation {
    pub const fn new() -> Self {
        Self(true)
    }

    pub const fn get(self) -> bool {
        self.0
    }
}

impl<'de> Deserialize<'de> for DiscardedContinuation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Ok(Self::new())
        } else {
            Err(serde::de::Error::custom(
                "discarded continuation must be true",
            ))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalPatch {
    Created {
        goal: SurfaceGoal,
    },
    Edited {
        goal_id: SurfaceGoalId,
        previous_revision: GoalRevision,
        goal: SurfaceGoal,
    },
    Removed {
        goal_id: SurfaceGoalId,
        previous_revision: GoalRevision,
        tombstone_revision: GoalRevision,
    },
    RunStarted {
        goal_id: SurfaceGoalId,
        goal_run: SurfaceGoalRun,
    },
    OuterTurnStarted {
        identity: SurfaceGoalGenerationIdentity,
    },
    IntentRequested {
        goal_id: SurfaceGoalId,
        outer_turn_id: SurfaceGoalOuterTurnId,
        intent: SurfaceGoalIntent,
    },
    IntentAcknowledged {
        goal_id: SurfaceGoalId,
        outer_turn_id: SurfaceGoalOuterTurnId,
        intent: SurfaceGoalIntent,
        ack: SurfaceGoalIntentAck,
    },
    OuterTurnFinished {
        identity: SurfaceGoalGenerationIdentity,
        status: GoalOuterTurnStatus,
        usage: GoalUsage,
        next_action: GoalOuterTurnNextAction,
    },
    VerificationCompleted {
        identity: SurfaceGoalGenerationIdentity,
        result: SurfaceGoalVerification,
    },
    Transitioned {
        goal_id: SurfaceGoalId,
        transition: SurfaceGoalTransition,
    },
    ContinuationDecided {
        goal_id: SurfaceGoalId,
        predecessor: SurfaceGoalGenerationIdentity,
        decision: GoalContinuationDecision,
    },
    Paused {
        goal_id: SurfaceGoalId,
        goal_run_id: Option<SurfaceGoalRunId>,
        outer_turn_id: Option<SurfaceGoalOuterTurnId>,
        state: SurfaceGoalState,
    },
    Recovered {
        goal_id: SurfaceGoalId,
        stale_run: SurfaceGoalClosedRunReceipt,
        recovery_message: DisplayText,
        discarded_continuation: DiscardedContinuation,
    },
    Completed {
        goal_id: SurfaceGoalId,
        goal_run_id: Option<SurfaceGoalRunId>,
        evidence: Vec<SurfaceEvidenceItem>,
        usage: GoalUsage,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GoalPatchEnvelope {
    pub receipt: SurfaceGoalStoreReceipt,
    pub patch: GoalPatch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpServerStatus {
    Starting,
    Ready,
    Degraded { message: DisplayText },
    Stopped,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMcpTool {
    pub id: SurfaceCatalogEntryId,
    pub server: NonEmptyText,
    pub name: NonEmptyText,
    pub schema_name: NonEmptyText,
    pub description: Option<DisplayText>,
    pub input_schema: SurfaceSchema,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMcpResource {
    pub id: SurfaceCatalogEntryId,
    pub server: NonEmptyText,
    pub uri: CanonicalUri,
    pub name: NonEmptyText,
    pub description: Option<DisplayText>,
    pub mime: Option<CanonicalMime>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMcpResourceTemplate {
    pub id: SurfaceCatalogEntryId,
    pub server: NonEmptyText,
    pub uri_template: NonEmptyText,
    pub name: NonEmptyText,
    pub description: Option<DisplayText>,
    pub mime: Option<CanonicalMime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpCatalogDiagnosticCode {
    EmptyName,
    EmptySchemaName,
    InvalidUri,
    InvalidUriTemplate,
    InvalidMime,
    InvalidSchema,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpCatalogEntryKind {
    Tool,
    Resource,
    ResourceTemplate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMcpCatalogDiagnostic {
    pub server: NonEmptyText,
    pub entry_kind: SurfaceMcpCatalogEntryKind,
    pub source_index: u64,
    pub code: SurfaceMcpCatalogDiagnosticCode,
    pub source_digest: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceMcpCatalogSnapshot {
    pub revision: McpCatalogRevision,
    pub servers: Vec<(NonEmptyText, SurfaceMcpServerStatus)>,
    pub tools: Vec<SurfaceMcpTool>,
    pub resources: Vec<SurfaceMcpResource>,
    pub resource_templates: Vec<SurfaceMcpResourceTemplate>,
    pub diagnostics: Vec<SurfaceMcpCatalogDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum McpCatalogPatch {
    Reconciled {
        previous_revision: McpCatalogRevision,
        snapshot: SurfaceMcpCatalogSnapshot,
    },
    ServerStatusChanged {
        previous_revision: McpCatalogRevision,
        next_revision: McpCatalogRevision,
        server: NonEmptyText,
        status: SurfaceMcpServerStatus,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePinnedContextKind {
    Memory,
    File,
    User,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePinnedContextEntry {
    pub id: SurfaceCatalogEntryId,
    pub kind: SurfacePinnedContextKind,
    pub label: NonEmptyText,
    pub content: DisplayText,
    pub content_digest: Sha256Digest,
    pub source_revision: PinnedContextSourceRevision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePinnedContextSnapshot {
    pub revision: PinnedContextRevision,
    pub entries: Vec<SurfacePinnedContextEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PinnedContextPatch {
    Added {
        previous_revision: PinnedContextRevision,
        next_revision: PinnedContextRevision,
        entry: SurfacePinnedContextEntry,
    },
    Removed {
        previous_revision: PinnedContextRevision,
        next_revision: PinnedContextRevision,
        entry_id: SurfaceCatalogEntryId,
    },
    Reconciled {
        previous_revision: PinnedContextRevision,
        snapshot: SurfacePinnedContextSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FirstOperationCompletionPolicy {
    Terminal,
    NotAdmitted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ThreadPersistence {
    RecordedCatalogued,
    EphemeralNonCataloguedOneShot {
        close_after: FirstOperationCompletionPolicy,
    },
    EphemeralAttached,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceThreadSnapshot {
    pub thread_id: SurfaceThreadId,
    pub owner_epoch: ThreadOwnerEpoch,
    pub persistence: ThreadPersistence,
    pub title: DisplayText,
    pub metadata_revision: SessionMetadataRevision,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub cwd: CanonicalPath,
    pub workspace_roots: Vec<CanonicalPath>,
    pub closed: bool,
}

#[derive(Clone, PartialEq)]
pub struct SurfaceBackgroundOperation {
    pub operation_id: SurfaceOperationId,
    pub fence: SurfaceBackgroundFence,
    pub task_id: Option<SurfaceTaskId>,
    pub transferred_at: SurfaceCursor,
    pub finalizing_degraded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceFactFamily {
    Operation,
    Item,
    Assistant,
    Tool,
    Plan,
    Usage,
    Context,
    Interaction,
    Task,
    Workflow,
    Subagent,
    Goal,
    Settings,
    McpCatalog,
    PinnedContext,
    Session,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceHealthIssueId {
    Mutation(SurfaceSettlementId),
    Projection(SurfaceCommitId),
    StartCommit(SurfaceCommitId),
    Finalization(SurfaceFinalizeIntentId),
    BackgroundFinalization(SurfaceFinalizeIntentId),
    Capability(SurfaceCapabilityCallId),
    RemoteTerminal(UuidV7),
    Ownership(ThreadOwnerEpoch),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceHealthIssue {
    MutationDegraded {
        settlement_id: SurfaceSettlementId,
    },
    ProjectionDegraded {
        commit_id: SurfaceCommitId,
        fact_family: SurfaceFactFamily,
    },
    StartCommitDegraded {
        fence: SurfaceOperationFence,
        commit_id: SurfaceCommitId,
    },
    FinalizingDegraded {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        cause: FinalizationDegradedCause,
    },
    BackgroundFinalizingDegraded {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        cause: FinalizationDegradedCause,
    },
    CapabilityObservationUnavailable {
        call_id: SurfaceCapabilityCallId,
    },
    ExternalEffectAmbiguous {
        call_id: SurfaceCapabilityCallId,
    },
    RemoteTerminalIdentityUnknown {
        lease_id: UuidV7,
    },
    RemoteTerminalCleanupAmbiguous {
        lease_id: UuidV7,
    },
    OwnershipLost {
        stale_epoch: ThreadOwnerEpoch,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSessionHealth {
    pub revision: SessionHealthRevision,
    pub accepting_admission: bool,
    pub issues: Vec<(SurfaceHealthIssueId, SurfaceHealthIssue)>,
    pub closing: bool,
    pub closed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceHealthClearProof {
    pub issue_id: SurfaceHealthIssueId,
    pub resolving_commit_id: SurfaceCommitId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionPatch {
    Materialized {
        thread: SurfaceThreadSnapshot,
    },
    OwnerEpochChanged {
        previous: ThreadOwnerEpoch,
        next: ThreadOwnerEpoch,
    },
    MetadataChanged {
        previous_revision: SessionMetadataRevision,
        next_revision: SessionMetadataRevision,
        title: DisplayText,
        updated_at: UnixMillis,
    },
    HealthIssueAdded {
        previous_revision: SessionHealthRevision,
        next_revision: SessionHealthRevision,
        id: SurfaceHealthIssueId,
        issue: SurfaceHealthIssue,
    },
    HealthIssueCleared {
        previous_revision: SessionHealthRevision,
        next_revision: SessionHealthRevision,
        id: SurfaceHealthIssueId,
        proof: SurfaceHealthClearProof,
    },
    RuntimeFault {
        class: FailureClass,
        message: DisplayText,
        causative_generation: Option<SurfaceOperationFence>,
    },
    Closing {
        reason: SurfaceShutdownReason,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        plan_digest: Sha256Digest,
    },
    Closed {
        reason: SurfaceShutdownReason,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        plan_digest: Sha256Digest,
    },
}
