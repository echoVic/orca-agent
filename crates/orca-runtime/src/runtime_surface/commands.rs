use super::hub::{
    AcpReadTextFileDispatchReceiver, AcpReadTextFileSettlement, AcpTerminalCleanupDispatchReceiver,
    AcpTerminalCleanupSettlement, AcpTerminalCreateDispatchReceiver, AcpTerminalCreateSettlement,
    AcpTerminalObservationDispatchReceiver, AcpTerminalObservationSettlement,
    AcpWriteTextFileDispatchReceiver, AcpWriteTextFileSettlement, SurfaceHub,
    SurfaceSubscriptionReceiver,
};
use super::identity::{
    BootstrapCredentialRevision, CanonicalPath, CanonicalUri, CapabilityRevision, CommitClass,
    ContextRevision, DisplayText, DurableRevision, DurationMillis, GoalCatalogRevision,
    GoalObjectiveRevision, GoalOwnerEpoch, GoalRevision, HostIncarnation, HostLifecycleRevision,
    HostRevisionWitness, InputCatalogRevision, InteractionRevision, McpCatalogRevision,
    MemoryRevision, NonEmptySet, NonEmptyText, NonEmptyVec, OpaqueToken,
    OptionalProcessLocalCancel, PinnedContextRevision, PlanRevision, PolicyEpoch,
    ProjectRootMemoryRevision, ResponseRouteEpoch, Rfc3339Timestamp, SequenceNumber,
    SessionCatalogRevision, SessionMetadataRevision, Set, SettingsRevision, Sha256Digest,
    SurfaceAdmissionLeaseId, SurfaceAttachmentGrant, SurfaceAttachmentId, SurfaceAttachmentRole,
    SurfaceBackgroundFence, SurfaceBoundCaller, SurfaceCapability, SurfaceCapabilityCallId,
    SurfaceCatalogEntryId, SurfaceCommitId, SurfaceConnectionId, SurfaceCursor, SurfaceEventId,
    SurfaceFinalizeIntentId, SurfaceGenerationId, SurfaceGoalFence, SurfaceGoalId,
    SurfaceHostBoundCaller, SurfaceIncarnation, SurfaceInteractionId, SurfaceItemId,
    SurfaceOperationFence, SurfaceOperationId, SurfaceRequestId, SurfaceResponseGrantToken,
    SurfaceResponseId, SurfaceResponseToken, SurfaceScope, SurfaceSettlementId, SurfaceTaskFence,
    SurfaceTaskId, SurfaceThreadId, SurfaceUnavailableReason, SurfaceValueError,
    SurfaceWorkflowFence, SurfaceWorkflowRunId, TaskRevision, ThreadOwnerEpoch, TrustRevision,
    UnixMillis, UsageRevision, UuidV7, WorkflowCatalogRevision, WorkflowRevision,
    ZeroizingProcessLocalSecret,
};
use super::interaction::{
    BoundInteractionResponse, BrokerInteractionAnswerPolicy, InteractionPatch,
    SurfaceClientInteractionAnswer, SurfaceDataValue, SurfaceInteractionKind,
    SurfaceInteractionResolutionReceipt, SurfaceInteractionView,
};
use super::operation::{
    FinalizationStartedAtCursor, GenerationPhase, InterruptSettlement, LastUserTurn, LegacyTurnId,
    OperationFinalizationCause, OperationKind, OperationPhase, OperationRecord,
    OperationRequestIntent, OperationTerminal, Replayability, ReservationLease,
    RuntimeSettingsPatch, SurfaceActivePermissionProfile, SurfaceAdditionalWorkingDirectory,
    SurfaceApprovalMode, SurfaceInputBindingKind, SurfaceInputRequest, SurfaceNetworkPermissions,
    SurfacePermissionRuleSet, SurfaceRuntimeSettings,
};
use super::projection::{
    AssistantPatch, FirstOperationCompletionPolicy, GoalPatchEnvelope, ItemPatch, McpCatalogPatch,
    OperationPatch, PinnedContextPatch, SessionPatch, SettingsPatch, SubagentPatch,
    SurfaceAssistantStream, SurfaceBackgroundOperation, SurfaceContextSnapshot, SurfaceFactFamily,
    SurfaceGoal, SurfaceGoalStoreReceipt, SurfaceItem, SurfaceMcpCatalogSnapshot,
    SurfaceMcpResource, SurfaceMcpResourceTemplate, SurfaceMcpTool, SurfacePinnedContextEntry,
    SurfacePinnedContextSnapshot, SurfacePlanSnapshot, SurfaceSessionHealth,
    SurfaceSettingsSnapshot, SurfaceSubagent, SurfaceTask, SurfaceThreadSnapshot, SurfaceToolView,
    SurfaceUsageSnapshot, SurfaceWorkflow, TaskPatch, ThreadPersistence, ToolInvocationStarted,
    ToolPatch, ToolTerminalSource, WorkflowPatch,
};
use super::reducer::canonical_replayability_digest;
use orca_core::budget::BudgetUsage;
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

pub const SURFACE_COMMIT_BATCH_EVENT_LIMIT: u64 = 1_024;
pub const SURFACE_COMMIT_BATCH_BYTE_LIMIT: u64 = 8_388_608;
pub const SURFACE_RETAINED_EVENT_LIMIT: u64 = 8_192;
pub const SURFACE_RETAINED_BYTE_LIMIT: u64 = 33_554_432;
pub const SURFACE_SUBSCRIBER_EVENT_LIMIT: u64 = 1_024;
pub const SURFACE_SUBSCRIBER_BYTE_LIMIT: u64 = 8_388_608;
pub const ACP_MAX_INBOUND_LINE_BYTES: u64 = 8_388_608;
pub const ACP_MAX_OUTBOUND_FRAME_BYTES: u64 = 8_388_608;
pub const ACP_INGRESS_MESSAGE_LIMIT: u64 = 64;
pub const ACP_INGRESS_BYTE_LIMIT: u64 = 16_777_216;
pub const ACP_OUTGOING_MESSAGE_LIMIT: u64 = 256;
pub const ACP_OUTGOING_BYTE_LIMIT: u64 = 33_554_432;
pub const ACP_LOAD_GATE_MESSAGE_LIMIT: u64 = 4_096;
pub const ACP_LOAD_GATE_BYTE_LIMIT: u64 = 67_108_864;
pub const ACP_PROMPT_GATE_MESSAGE_LIMIT: u64 = 1_024;
pub const ACP_PROMPT_GATE_BYTE_LIMIT: u64 = 16_777_216;
pub const ACP_WRITE_FLUSH_DEADLINE_MS: u64 = 30_000;
pub const ACP_REVERSE_REQUEST_DEADLINE_MS: u64 = 120_000;
pub const ACP_CAPABILITY_CALL_DEADLINE_MS: u64 = 60_000;
pub const ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT: u64 = 4_194_304;
pub const ACP_TERMINAL_KILL_DEADLINE_MS: u64 = 10_000;
pub const ACP_TERMINAL_RELEASE_DEADLINE_MS: u64 = 10_000;
pub const ACP_SUPERVISOR_JOIN_DEADLINE_MS: u64 = 5_000;
pub const ACP_TOMBSTONE_TTL_MS: u64 = 300_000;
pub const ACP_TOMBSTONE_LIMIT: u64 = 4_096;
pub const JSONL_REQUEST_TOMBSTONE_TTL_MS: u64 = 300_000;
pub const JSONL_REQUEST_TOMBSTONE_LIMIT: u64 = 4_096;
pub const JSONL_LIVE_REQUEST_LIMIT: u64 = 1_024;
pub const JSONL_REPAIR_AUTHORITY_LIMIT: u64 = 1_024;
pub const JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS: u64 = 5_000;
pub const JSONL_SUPERVISOR_JOIN_DEADLINE_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AcpStandardCapabilitySet {
    pub session_usage: bool,
    pub session_model: bool,
    pub session_modes: bool,
    pub session_info: bool,
    pub file_read: bool,
    pub file_write: bool,
    pub terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AcpAttachmentCapabilityProfile {
    pub(crate) revision: CapabilityRevision,
    pub(crate) standard: AcpStandardCapabilitySet,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceEvent {
    Operation(OperationPatch),
    Item(ItemPatch),
    Assistant(AssistantPatch),
    Tool(ToolPatch),
    Plan(SurfacePlanSnapshot),
    Usage(SurfaceUsageSnapshot),
    Context(SurfaceContextSnapshot),
    Interaction(InteractionPatch),
    Task(TaskPatch),
    Workflow(WorkflowPatch),
    Subagent(SubagentPatch),
    Goal(GoalPatchEnvelope),
    Settings(SettingsPatch),
    McpCatalog(McpCatalogPatch),
    PinnedContext(PinnedContextPatch),
    Session(SessionPatch),
}

#[derive(Clone, PartialEq)]
pub struct SurfaceEventEnvelope {
    pub ordinal: u32,
    pub event_id: SurfaceEventId,
    pub commit_class: CommitClass,
    pub scope: SurfaceScope,
    pub event: SurfaceEvent,
}

#[derive(Clone, PartialEq)]
pub struct SurfaceCommitBatch {
    pub cursor_before: SurfaceCursor,
    pub cursor_after: SurfaceCursor,
    pub commit_class: CommitClass,
    pub event_count: u32,
    pub batch_digest: Sha256Digest,
    pub events: NonEmptyVec<SurfaceEventEnvelope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceCommitBatchPreflightResult {
    Ready {
        event_count: u32,
        canonical_encoded_bytes: u64,
        batch_digest: Sha256Digest,
    },
    Rejected {
        code: SurfaceCommitBatchPreflightErrorCode,
        observed_event_count: u64,
        observed_canonical_encoded_bytes: u64,
        event_limit: u64,
        byte_limit: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceCommitBatchPreflightErrorCode {
    CommitBatchTooLarge,
}

#[derive(Clone, PartialEq)]
pub struct SurfaceSnapshot {
    pub cursor: SurfaceCursor,
    pub thread: SurfaceThreadSnapshot,
    pub foreground_operation: Option<OperationRecord>,
    pub queued_operations: Vec<OperationRecord>,
    pub background_operations: Vec<SurfaceBackgroundOperation>,
    pub operation_history: Vec<OperationRecord>,
    pub items: Vec<SurfaceItem>,
    pub assistant_streams: Vec<SurfaceAssistantStream>,
    pub tools: Vec<SurfaceToolView>,
    pub plan: SurfacePlanSnapshot,
    pub usage: SurfaceUsageSnapshot,
    pub context: SurfaceContextSnapshot,
    pub interactions: Vec<SurfaceInteractionView>,
    pub tasks: Vec<SurfaceTask>,
    pub workflows: Vec<SurfaceWorkflow>,
    pub subagents: Vec<SurfaceSubagent>,
    pub goal: Option<SurfaceGoal>,
    pub settings: SurfaceSettingsSnapshot,
    pub mcp_catalog: SurfaceMcpCatalogSnapshot,
    pub pinned_context: SurfacePinnedContextSnapshot,
    pub session_health: SurfaceSessionHealth,
}

impl SurfaceSnapshot {
    pub fn recoverable_user_operation(&self) -> Option<SurfaceRecoverableOperation> {
        let operation = self.foreground_operation.as_ref()?;
        if !matches!(operation.phase, OperationPhase::Suspended { .. })
            || operation.pending_control.is_some()
            || operation.finalization.is_some()
            || operation.terminal.is_some()
            || !matches!(
                operation.intent.kind,
                OperationKind::UserTurn | OperationKind::GoalRun { .. }
            )
        {
            return None;
        }
        let generation = operation.generations.last()?;
        if generation.phase != GenerationPhase::Stopped {
            return None;
        }
        let resume_source = match &generation.replayability {
            Replayability::Replayable { .. } => ResumeSourceWitness::DurableReplay {
                replayability_digest: canonical_replayability_digest(&generation.replayability),
            },
            Replayability::NonReplayable { .. } => return None,
        };
        Some(SurfaceRecoverableOperation {
            operation_id: operation.operation_id.clone(),
            expected_last_generation: generation.fence.generation_id,
            resume_source,
        })
    }
}

#[derive(Clone, PartialEq)]
pub struct SnapshotAtCursor {
    pub snapshot: Arc<SurfaceSnapshot>,
    pub cursor: SurfaceCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceAttachmentCapabilities {
    pub grant: SurfaceAttachmentGrant,
    pub interaction_kinds: Set<SurfaceInteractionKind>,
    pub acp_capability_revision: Option<CapabilityRevision>,
}

#[allow(dead_code)]
#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceAttachAuthority {
    host_incarnation: HostIncarnation,
    thread_id: SurfaceThreadId,
    role: SurfaceAttachmentRole,
    maximum_capabilities: NonEmptySet<SurfaceCapability>,
    required_capabilities: NonEmptySet<SurfaceCapability>,
    maximum_interaction_kinds: Set<SurfaceInteractionKind>,
    connection_id: Option<SurfaceConnectionId>,
}

#[allow(dead_code)]
impl SurfaceAttachAuthority {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        thread_id: SurfaceThreadId,
        role: SurfaceAttachmentRole,
        maximum_capabilities: NonEmptySet<SurfaceCapability>,
        required_capabilities: NonEmptySet<SurfaceCapability>,
        maximum_interaction_kinds: Set<SurfaceInteractionKind>,
    ) -> Self {
        Self {
            host_incarnation,
            thread_id,
            role,
            maximum_capabilities,
            required_capabilities,
            maximum_interaction_kinds,
            connection_id: None,
        }
    }

    pub(crate) fn host_incarnation(&self) -> &HostIncarnation {
        &self.host_incarnation
    }

    pub(crate) fn thread_id(&self) -> &SurfaceThreadId {
        &self.thread_id
    }

    pub(crate) fn role(&self) -> SurfaceAttachmentRole {
        self.role
    }

    pub(crate) fn maximum_capabilities(&self) -> &NonEmptySet<SurfaceCapability> {
        &self.maximum_capabilities
    }

    pub(crate) fn required_capabilities(&self) -> &NonEmptySet<SurfaceCapability> {
        &self.required_capabilities
    }

    pub(crate) fn maximum_interaction_kinds(&self) -> &Set<SurfaceInteractionKind> {
        &self.maximum_interaction_kinds
    }

    pub(crate) fn connection_id(&self) -> Option<&SurfaceConnectionId> {
        self.connection_id.as_ref()
    }

    pub(crate) fn with_connection_id(mut self, connection_id: SurfaceConnectionId) -> Self {
        self.connection_id = Some(connection_id);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachDeniedReason {
    RoleMismatch,
    MissingRequiredCapability,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct SurfaceSubscriptionHandle(Arc<SurfaceSubscriptionLease>);

struct SurfaceSubscriptionLease {
    attachment_id: SurfaceAttachmentId,
    reclaim: Box<dyn Fn(&SurfaceAttachmentId) + Send + Sync>,
}

impl Drop for SurfaceSubscriptionLease {
    fn drop(&mut self) {
        (self.reclaim)(&self.attachment_id);
    }
}

#[allow(dead_code)]
impl SurfaceSubscriptionHandle {
    pub(crate) fn new(
        attachment_id: SurfaceAttachmentId,
        reclaim: impl Fn(&SurfaceAttachmentId) + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(SurfaceSubscriptionLease {
            attachment_id,
            reclaim: Box::new(reclaim),
        }))
    }

    pub(crate) fn attachment_id(&self) -> &SurfaceAttachmentId {
        &self.0.attachment_id
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceClientHandle {
    attachment_id: SurfaceAttachmentId,
    thread_id: SurfaceThreadId,
    host_incarnation: HostIncarnation,
    capabilities: SurfaceAttachmentGrant,
    connection_id: Option<SurfaceConnectionId>,
    hub_scope: Arc<()>,
    detached_receipt: Arc<Mutex<Option<DetachRevocationReceipt>>>,
    dispatcher: Option<Arc<dyn RuntimeSurfaceCommandDispatcher>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceClientCommandError {
    RuntimeUnavailable,
    Unauthorized,
}

pub(crate) trait RuntimeSurfaceCommandDispatcher: Send + Sync {
    fn notify_interaction_capability_changed(&self);

    fn prompt_queue(
        &self,
        client: RuntimeSurfaceClientHandle,
        action: crate::prompt_queue::PromptQueueAction,
    ) -> Result<
        crate::prompt_queue::PromptQueueSnapshot,
        crate::prompt_queue::PromptQueueMutationError,
    >;

    fn claim_acp_read_text_file_write(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn mark_acp_read_text_file_written(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn settle_acp_read_text_file(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpReadTextFileSettlement,
    ) -> Result<(), SurfaceClientCommandError>;

    fn permit_acp_write_text_file_delivery(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn mark_acp_write_text_file_written(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn settle_acp_write_text_file(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpWriteTextFileSettlement,
    ) -> Result<(), SurfaceClientCommandError>;

    fn permit_acp_terminal_create_delivery(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn mark_acp_terminal_create_written(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn settle_acp_terminal_create(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalCreateSettlement,
    ) -> Result<(), SurfaceClientCommandError>;

    fn claim_acp_terminal_observation_write(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn mark_acp_terminal_observation_written(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn settle_acp_terminal_observation(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalObservationSettlement,
    ) -> Result<(), SurfaceClientCommandError>;

    fn mark_acp_terminal_cleanup_written(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError>;

    fn settle_acp_terminal_cleanup(
        &self,
        client: RuntimeSurfaceClientHandle,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalCleanupSettlement,
    ) -> Result<(), SurfaceClientCommandError>;

    fn detach(&self, client: RuntimeSurfaceClientHandle, request: DetachRequest) -> DetachResult;

    fn reserve_operation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        intent: OperationRequestIntent,
    ) -> Result<MutationReply<ReservedOperationOutput>, SurfaceClientCommandError>;

    fn admit_reserved(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    ) -> Result<MutationReply<AdmissionOutput>, SurfaceClientCommandError>;

    fn admit_reserved_with_output(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
        writer: Box<dyn crate::runtime_host::HostedOperationWriter>,
    ) -> Result<MutationReply<AdmissionOutput>, SurfaceClientCommandError>;

    fn cancel_operation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
    ) -> Result<MutationReply<CancelOperationOutput>, SurfaceClientCommandError>;

    fn cancel_acp_prompt_binding(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        session_id: NonEmptyText,
        inbound_seq: SequenceNumber,
    ) -> Result<CancelSessionCurrentResult, SurfaceClientCommandError>;

    fn transfer_background(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        target: BackgroundTarget,
    ) -> Result<MutationReply<TransferBackgroundOutput>, SurfaceClientCommandError>;

    fn pause_goal_operation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        goal_fence: SurfaceGoalFence,
    ) -> Result<MutationReply<PauseGoalOutput>, SurfaceClientCommandError>;

    fn resume_operation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        expected_last_generation: SurfaceGenerationId,
        resume_source: ResumeSourceWitness,
    ) -> Result<MutationReply<ResumeOperationOutput>, SurfaceClientCommandError>;

    fn wait_operation_terminal_with_cancel(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        caller_cancel: OptionalProcessLocalCancel,
    ) -> Result<WaitOperationTerminalResult, SurfaceClientCommandError>;

    fn manual_compact(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        expected_context_revision: ContextRevision,
    ) -> Result<MutationReply<MaintenanceOperationOutput>, SurfaceClientCommandError>;

    fn update_settings(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        expected_thread_revision: SettingsRevision,
        patch: NonEmptyVec<RuntimeSettingsPatch>,
    ) -> Result<MutationReply<SettingsMutationOutput>, SurfaceClientCommandError>;

    fn update_session_metadata(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        precondition: SessionMetadataPrecondition,
        patch: SessionMetadataPatch,
    ) -> Result<MutationReply<()>, SurfaceClientCommandError>;

    fn pinned_context_mutation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        action: PinnedContextAction,
    ) -> Result<MutationReply<PinnedContextMutationOutput>, SurfaceClientCommandError>;

    fn goal_mutation(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        action: GoalMutationAction,
    ) -> Result<MutationReply<GoalMutationOutput>, SurfaceClientCommandError>;

    fn workflow_control(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        action: WorkflowControlAction,
    ) -> Result<MutationReply<WorkflowControlOutput>, SurfaceClientCommandError>;

    fn task_control(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        action: TaskControlAction,
    ) -> Result<MutationReply<TaskControlOutput>, SurfaceClientCommandError>;

    fn read_task_transcript(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
    ) -> Result<SurfaceReadResult<TaskTranscriptSnapshot>, SurfaceClientCommandError>;

    fn respond_interaction_by_id(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        interaction_id: SurfaceInteractionId,
        answer: SurfaceClientInteractionAnswer,
    ) -> Result<MutationReply<RespondInteractionOutput>, SurfaceClientCommandError>;

    fn respond_interaction_by_id_with_policy(
        &self,
        client: RuntimeSurfaceClientHandle,
        request_id: SurfaceRequestId,
        interaction_id: SurfaceInteractionId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
    ) -> Result<MutationReply<RespondInteractionOutput>, SurfaceClientCommandError>;

    fn retry_finalization(
        &self,
        client: RuntimeSurfaceClientHandle,
        token: RetryFinalizationToken,
    ) -> Result<MutationReply<OperationTerminalAtCursor>, SurfaceClientCommandError>;
}

#[allow(dead_code)]
impl RuntimeSurfaceClientHandle {
    pub fn prompt_queue(
        &self,
        action: crate::prompt_queue::PromptQueueAction,
    ) -> Result<
        crate::prompt_queue::PromptQueueSnapshot,
        crate::prompt_queue::PromptQueueMutationError,
    > {
        self.dispatcher
            .as_ref()
            .ok_or(crate::prompt_queue::PromptQueueMutationError::RuntimeUnavailable)?
            .prompt_queue(self.clone(), action)
    }

    pub(crate) fn claim_acp_read_text_file_write(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .claim_acp_read_text_file_write(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn new(
        attachment_id: SurfaceAttachmentId,
        thread_id: SurfaceThreadId,
        host_incarnation: HostIncarnation,
        capabilities: SurfaceAttachmentGrant,
        connection_id: Option<SurfaceConnectionId>,
        hub_scope: Arc<()>,
    ) -> Self {
        Self {
            attachment_id,
            thread_id,
            host_incarnation,
            capabilities,
            connection_id,
            hub_scope,
            detached_receipt: Arc::new(Mutex::new(None)),
            dispatcher: None,
        }
    }

    pub(crate) fn with_dispatcher(
        mut self,
        dispatcher: Option<Arc<dyn RuntimeSurfaceCommandDispatcher>>,
    ) -> Self {
        self.dispatcher = dispatcher;
        self
    }

    pub(crate) fn mark_acp_read_text_file_written(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .mark_acp_read_text_file_written(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn settle_acp_read_text_file(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpReadTextFileSettlement,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .settle_acp_read_text_file(self.clone(), call_id, capability_revision, settlement)
    }

    pub(crate) fn permit_acp_write_text_file_delivery(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .permit_acp_write_text_file_delivery(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn mark_acp_write_text_file_written(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .mark_acp_write_text_file_written(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn settle_acp_write_text_file(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpWriteTextFileSettlement,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .settle_acp_write_text_file(self.clone(), call_id, capability_revision, settlement)
    }

    pub(crate) fn permit_acp_terminal_create_delivery(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .permit_acp_terminal_create_delivery(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn mark_acp_terminal_create_written(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .mark_acp_terminal_create_written(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn settle_acp_terminal_create(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalCreateSettlement,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .settle_acp_terminal_create(self.clone(), call_id, capability_revision, settlement)
    }

    pub(crate) fn claim_acp_terminal_observation_write(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .claim_acp_terminal_observation_write(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn mark_acp_terminal_observation_written(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .mark_acp_terminal_observation_written(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn settle_acp_terminal_observation(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalObservationSettlement,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .settle_acp_terminal_observation(self.clone(), call_id, capability_revision, settlement)
    }

    pub(crate) fn mark_acp_terminal_cleanup_written(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .mark_acp_terminal_cleanup_written(self.clone(), call_id, capability_revision)
    }

    pub(crate) fn settle_acp_terminal_cleanup(
        &self,
        call_id: SurfaceCapabilityCallId,
        capability_revision: CapabilityRevision,
        settlement: AcpTerminalCleanupSettlement,
    ) -> Result<(), SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .settle_acp_terminal_cleanup(self.clone(), call_id, capability_revision, settlement)
    }

    pub fn reserve_operation(
        &self,
        request_id: SurfaceRequestId,
        intent: OperationRequestIntent,
    ) -> Result<MutationReply<ReservedOperationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .reserve_operation(self.clone(), request_id, intent)
    }

    pub fn admit_reserved(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    ) -> Result<MutationReply<AdmissionOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .admit_reserved(self.clone(), request_id, operation_id, admission_lease_id)
    }

    pub fn admit_reserved_with_output<W>(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
        writer: W,
    ) -> Result<MutationReply<AdmissionOutput>, SurfaceClientCommandError>
    where
        W: crate::runtime_host::HostedOperationWriter,
    {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .admit_reserved_with_output(
                self.clone(),
                request_id,
                operation_id,
                admission_lease_id,
                Box::new(writer),
            )
    }

    pub fn cancel_operation(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
    ) -> Result<MutationReply<CancelOperationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .cancel_operation(self.clone(), request_id, operation_id)
    }

    pub(crate) fn cancel_acp_prompt_binding(
        &self,
        request_id: SurfaceRequestId,
        session_id: NonEmptyText,
        inbound_seq: SequenceNumber,
    ) -> Result<CancelSessionCurrentResult, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .cancel_acp_prompt_binding(self.clone(), request_id, session_id, inbound_seq)
    }

    pub fn transfer_background(
        &self,
        request_id: SurfaceRequestId,
        target: BackgroundTarget,
    ) -> Result<MutationReply<TransferBackgroundOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .transfer_background(self.clone(), request_id, target)
    }

    pub fn pause_goal_operation(
        &self,
        request_id: SurfaceRequestId,
        goal_fence: SurfaceGoalFence,
    ) -> Result<MutationReply<PauseGoalOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .pause_goal_operation(self.clone(), request_id, goal_fence)
    }

    pub fn resume_operation(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        expected_last_generation: SurfaceGenerationId,
        resume_source: ResumeSourceWitness,
    ) -> Result<MutationReply<ResumeOperationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .resume_operation(
                self.clone(),
                request_id,
                operation_id,
                expected_last_generation,
                resume_source,
            )
    }

    pub fn resume_recoverable(
        &self,
        request_id: SurfaceRequestId,
        recovery: SurfaceRecoverableOperation,
    ) -> Result<MutationReply<ResumeOperationOutput>, SurfaceClientCommandError> {
        self.resume_operation(
            request_id,
            recovery.operation_id,
            recovery.expected_last_generation,
            recovery.resume_source,
        )
    }

    pub fn wait_operation_terminal(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
    ) -> Result<WaitOperationTerminalResult, SurfaceClientCommandError> {
        self.wait_operation_terminal_with_cancel(
            request_id,
            operation_id,
            OptionalProcessLocalCancel::new(),
        )
    }

    pub fn wait_operation_terminal_with_cancel(
        &self,
        request_id: SurfaceRequestId,
        operation_id: SurfaceOperationId,
        caller_cancel: OptionalProcessLocalCancel,
    ) -> Result<WaitOperationTerminalResult, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .wait_operation_terminal_with_cancel(
                self.clone(),
                request_id,
                operation_id,
                caller_cancel,
            )
    }

    pub fn manual_compact(
        &self,
        request_id: SurfaceRequestId,
        expected_context_revision: ContextRevision,
    ) -> Result<MutationReply<MaintenanceOperationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .manual_compact(self.clone(), request_id, expected_context_revision)
    }

    pub fn update_settings(
        &self,
        request_id: SurfaceRequestId,
        expected_thread_revision: SettingsRevision,
        patch: NonEmptyVec<RuntimeSettingsPatch>,
    ) -> Result<MutationReply<SettingsMutationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .update_settings(self.clone(), request_id, expected_thread_revision, patch)
    }

    pub fn update_session_metadata(
        &self,
        request_id: SurfaceRequestId,
        precondition: SessionMetadataPrecondition,
        patch: SessionMetadataPatch,
    ) -> Result<MutationReply<()>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .update_session_metadata(self.clone(), request_id, precondition, patch)
    }

    pub fn pinned_context_mutation(
        &self,
        request_id: SurfaceRequestId,
        action: PinnedContextAction,
    ) -> Result<MutationReply<PinnedContextMutationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .pinned_context_mutation(self.clone(), request_id, action)
    }

    pub fn goal_mutation(
        &self,
        request_id: SurfaceRequestId,
        action: GoalMutationAction,
    ) -> Result<MutationReply<GoalMutationOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .goal_mutation(self.clone(), request_id, action)
    }

    pub fn workflow_control(
        &self,
        request_id: SurfaceRequestId,
        action: WorkflowControlAction,
    ) -> Result<MutationReply<WorkflowControlOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .workflow_control(self.clone(), request_id, action)
    }

    pub fn task_control(
        &self,
        request_id: SurfaceRequestId,
        action: TaskControlAction,
    ) -> Result<MutationReply<TaskControlOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .task_control(self.clone(), request_id, action)
    }

    /// Reads a child transcript through the actor-owned typed surface.
    ///
    /// The request is fenced by the actor-owned surface task revision; callers must
    /// treat a stale result as a signal to refresh the projected task list.
    pub fn read_task_transcript(
        &self,
        request_id: SurfaceRequestId,
        task_id: SurfaceTaskId,
        expected_revision: TaskRevision,
    ) -> Result<SurfaceReadResult<TaskTranscriptSnapshot>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .read_task_transcript(self.clone(), request_id, task_id, expected_revision)
    }

    pub fn respond_interaction(
        &self,
        _request_id: SurfaceRequestId,
        _opaque_request_id: NonEmptyText,
        _expected_kind: SurfaceInteractionKind,
        _answer: SurfaceClientInteractionAnswer,
    ) -> Result<MutationReply<RespondInteractionOutput>, SurfaceClientCommandError> {
        Err(SurfaceClientCommandError::Unauthorized)
    }

    pub fn respond_interaction_by_id(
        &self,
        request_id: SurfaceRequestId,
        interaction_id: SurfaceInteractionId,
        answer: SurfaceClientInteractionAnswer,
    ) -> Result<MutationReply<RespondInteractionOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .respond_interaction_by_id(self.clone(), request_id, interaction_id, answer)
    }

    pub(crate) fn respond_interaction_by_id_with_policy(
        &self,
        request_id: SurfaceRequestId,
        interaction_id: SurfaceInteractionId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
    ) -> Result<MutationReply<RespondInteractionOutput>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .respond_interaction_by_id_with_policy(
                self.clone(),
                request_id,
                interaction_id,
                answer,
                policy,
            )
    }

    pub fn retry_finalization(
        &self,
        token: RetryFinalizationToken,
    ) -> Result<MutationReply<OperationTerminalAtCursor>, SurfaceClientCommandError> {
        self.dispatcher
            .as_ref()
            .ok_or(SurfaceClientCommandError::RuntimeUnavailable)?
            .retry_finalization(self.clone(), token)
    }

    pub(crate) fn attachment_id(&self) -> &SurfaceAttachmentId {
        &self.attachment_id
    }

    pub(crate) fn connection_id(&self) -> Option<&SurfaceConnectionId> {
        self.connection_id.as_ref()
    }

    pub(crate) fn thread_id(&self) -> &SurfaceThreadId {
        &self.thread_id
    }

    pub(crate) fn grant(&self) -> &SurfaceAttachmentGrant {
        &self.capabilities
    }

    pub(crate) fn belongs_to(
        &self,
        hub_scope: &Arc<()>,
        thread_id: &SurfaceThreadId,
        host_incarnation: &HostIncarnation,
    ) -> bool {
        Arc::ptr_eq(&self.hub_scope, hub_scope)
            && &self.thread_id == thread_id
            && &self.host_incarnation == host_incarnation
            && self.attachment_id == self.capabilities.attachment_id
    }

    pub(crate) fn detached_receipt(&self) -> Option<DetachRevocationReceipt> {
        self.detached_receipt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn remember_detached(
        &self,
        receipt: DetachRevocationReceipt,
    ) -> Result<(), DetachRevocationReceipt> {
        let mut detached = self
            .detached_receipt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = detached.as_ref() {
            return Err(existing.clone());
        }
        *detached = Some(receipt);
        Ok(())
    }
}

#[derive(Clone)]
pub struct FreshSurfaceAttachment {
    pub attachment_id: SurfaceAttachmentId,
    pub client: RuntimeSurfaceClientHandle,
    pub baseline: SnapshotAtCursor,
    pub subscription: SurfaceSubscriptionHandle,
    pub capabilities: SurfaceAttachmentCapabilities,
}

#[derive(Clone)]
pub struct CursorSurfaceAttachment {
    pub attachment_id: SurfaceAttachmentId,
    pub client: RuntimeSurfaceClientHandle,
    pub from: SurfaceCursor,
    pub head: SurfaceCursor,
    pub replay: Vec<SurfaceCommitBatch>,
    pub subscription: SurfaceSubscriptionHandle,
    pub capabilities: SurfaceAttachmentCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreshAttachRequest {
    pub request_id: SurfaceRequestId,
    pub role: SurfaceAttachmentRole,
    pub requested_capabilities: Set<SurfaceCapability>,
    pub interaction_capabilities: Set<SurfaceInteractionKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorAttachRequest {
    pub request_id: SurfaceRequestId,
    pub cursor: SurfaceCursor,
    pub role: SurfaceAttachmentRole,
    pub requested_capabilities: Set<SurfaceCapability>,
    pub interaction_capabilities: Set<SurfaceInteractionKind>,
}

#[derive(Clone)]
pub enum AttachResult {
    FreshAttached { attachment: FreshSurfaceAttachment },
    CursorAttached { attachment: CursorSurfaceAttachment },
    Denied { reason: AttachDeniedReason },
    SnapshotRequired { required: SnapshotRequired },
    InvalidCursor { error: InvalidCursor },
    ThreadClosed { thread_id: SurfaceThreadId },
    Unavailable { reason: SurfaceUnavailableReason },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotRequiredReason {
    StaleIncarnation,
    ExpiredSuffix,
    ReplayHole,
    SlowSubscriber,
    ProjectionReset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRequired {
    pub reason: SnapshotRequiredReason,
    pub retained_from: Option<SurfaceCursor>,
    pub head: SurfaceCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCursorReason {
    WrongThread,
    FutureSequence,
    ImpossibleSourceRevision,
    NotBatchBoundary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidCursor {
    pub reason: InvalidCursorReason,
    pub supplied: SurfaceCursor,
    pub expected_thread: SurfaceThreadId,
    pub head: SurfaceCursor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSubscriptionSealReason {
    ThreadClosed,
    HostShutdown,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceSubscriptionItem {
    Batch {
        batch: SurfaceCommitBatch,
    },
    Gap {
        required: SnapshotRequired,
    },
    Sealed {
        reason: SurfaceSubscriptionSealReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachRequest {
    pub request_id: SurfaceRequestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachRevocationReceipt {
    pub request_id: SurfaceRequestId,
    pub attachment_id: SurfaceAttachmentId,
    pub revoked_grant_digest: Sha256Digest,
    pub affected_route_epochs: Vec<(SurfaceInteractionId, ResponseRouteEpoch)>,
    pub route_commit_id: Option<SurfaceCommitId>,
    pub route_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, PartialEq)]
pub enum DetachResult {
    Detached {
        receipt: DetachRevocationReceipt,
    },
    AlreadyDetached {
        receipt: DetachRevocationReceipt,
    },
    Deferred {
        receipt: DetachRevocationReceipt,
        mutation: DeferredMutation,
    },
    StaleAttachment {
        request_id: SurfaceRequestId,
        attachment_id: SurfaceAttachmentId,
    },
}

#[derive(Clone)]
pub struct WaitOperationTerminalRequest {
    pub request_id: SurfaceRequestId,
    pub operation_id: SurfaceOperationId,
    pub caller_cancel: OptionalProcessLocalCancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminalAtCursor {
    pub operation_id: SurfaceOperationId,
    pub terminal: OperationTerminal,
    pub completion_proof: super::projection::SurfaceOperationCompletionProof,
    pub cursor: SurfaceCursor,
    pub commit_class: CommitClass,
    pub batch_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
pub enum WaitOperationTerminalResult {
    Terminal {
        value: OperationTerminalAtCursor,
    },
    TerminalCommitFailure {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        commit_id: SurfaceCommitId,
        repair: RetryFinalizationToken,
    },
    TerminalProjectionFailure {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        terminal_event_id: SurfaceEventId,
        repair: RetryProjectionToken,
    },
    UnknownOperation {
        operation_id: SurfaceOperationId,
    },
    WrongThread {
        operation_id: SurfaceOperationId,
    },
    WaitCancelled {
        operation_id: SurfaceOperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationMemoryScope {
    User,
    Project { root: CanonicalPath },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationTarget {
    Host {
        host_incarnation: HostIncarnation,
    },
    Thread {
        thread_id: SurfaceThreadId,
    },
    Operation {
        thread_id: SurfaceThreadId,
        operation_id: SurfaceOperationId,
    },
    Generation {
        fence: SurfaceOperationFence,
    },
    Interaction {
        thread_id: SurfaceThreadId,
        interaction_id: SurfaceInteractionId,
    },
    Goal {
        goal_id: SurfaceGoalId,
    },
    Task {
        thread_id: SurfaceThreadId,
        task_id: SurfaceTaskId,
    },
    Workflow {
        thread_id: SurfaceThreadId,
        workflow_run_id: SurfaceWorkflowRunId,
    },
    Memory {
        scope: MutationMemoryScope,
    },
    FolderTrust {
        path: CanonicalPath,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationDisposition {
    Accepted,
    Queued,
    AlreadyApplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDomainKind {
    Memory,
    FolderTrust,
    RuntimeSettings,
    SessionCatalog,
    SessionMetadata,
    HostLifecycle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRevocationBarrierPlan {
    pub canonical_path: CanonicalPath,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
    pub expected_owner_leases: Vec<UuidV7>,
    pub expected_resources: Vec<NonEmptyText>,
    pub plan_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadCursorAckRequirement {
    pub thread_id: SurfaceThreadId,
    pub family: SurfaceFactFamily,
    pub event_id: SurfaceEventId,
    pub commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostReceiptRequirementIdentity {
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
    },
    FolderTrust {
        path: CanonicalPath,
        revision: TrustRevision,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostReceiptAckRequirement {
    pub host_incarnation: HostIncarnation,
    pub identity: HostReceiptRequirementIdentity,
    pub commit_id: SurfaceCommitId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminalAckRequirement {
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub operation_id: SurfaceOperationId,
    pub terminal_commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationAckRequirement {
    ThreadCursor(ThreadCursorAckRequirement),
    ThreadRemoteOwner {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    HostReceipt(HostReceiptAckRequirement),
    GoalStoreReceipt {
        goal_id: SurfaceGoalId,
        store_commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
    OperationTerminal(OperationTerminalAckRequirement),
    PolicyRevocationBarrier {
        plan: PolicyRevocationBarrierPlan,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderTrustLevel {
    Trusted,
    Untrusted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceMemoryReceipt {
    pub scope: MutationMemoryScope,
    pub record_id: SurfaceCatalogEntryId,
    pub memory_revision: MemoryRevision,
    pub display_path: CanonicalPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceFolderTrustReceipt {
    pub canonical_path: CanonicalPath,
    pub old_effective_level: FolderTrustLevel,
    pub new_effective_level: FolderTrustLevel,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
    pub reload_required: bool,
    pub reconciliation_proof: Option<Sha256Digest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceRuntimeSettingsReceipt {
    pub host_revision: SettingsRevision,
    pub thread_revision: Option<SettingsRevision>,
    pub effective: SurfaceRuntimeSettings,
    pub pending: Option<SurfaceRuntimeSettings>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSessionCatalogAction {
    Created,
    Opened,
    Loaded,
    Forked,
    Closed,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionCatalogReceipt {
    pub catalog_revision: SessionCatalogRevision,
    pub thread_id: Option<SurfaceThreadId>,
    pub action: SurfaceSessionCatalogAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionMetadataReceipt {
    pub thread_id: SurfaceThreadId,
    pub metadata_revision: SessionMetadataRevision,
    pub title: DisplayText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHostShutdownStage {
    Last,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceHostShutdownReceipt {
    pub host_incarnation: HostIncarnation,
    pub lifecycle_revision: HostLifecycleRevision,
    pub barrier_id: SurfaceSettlementId,
    pub shutdown_commit_id: SurfaceCommitId,
    pub stage: SurfaceHostShutdownStage,
    pub closed_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostDomainReceipt {
    Memory(SurfaceMemoryReceipt),
    FolderTrust(SurfaceFolderTrustReceipt),
    RuntimeSettings(SurfaceRuntimeSettingsReceipt),
    SessionCatalog(SurfaceSessionCatalogReceipt),
    SessionMetadata(SurfaceSessionMetadataReceipt),
    HostLifecycle(SurfaceHostShutdownReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostReceiptIdentityPair {
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
        receipt: SurfaceMemoryReceipt,
    },
    FolderTrust {
        path: CanonicalPath,
        revision: TrustRevision,
        receipt: SurfaceFolderTrustReceipt,
    },
    RuntimeSettings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
        receipt: SurfaceRuntimeSettingsReceipt,
    },
    SessionCatalog {
        thread_id: Option<SurfaceThreadId>,
        revision: SessionCatalogRevision,
        receipt: SurfaceSessionCatalogReceipt,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
        receipt: SurfaceSessionMetadataReceipt,
    },
    HostLifecycle {
        host_incarnation: HostIncarnation,
        revision: HostLifecycleRevision,
        receipt: SurfaceHostShutdownReceipt,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationCommitAck {
    ThreadLocalCursor {
        cursor: SurfaceCursor,
        family: SurfaceFactFamily,
        event_id: SurfaceEventId,
        commit_class: CommitClass,
    },
    ThreadRemoteOwnerAck {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
        commit_id: SurfaceCommitId,
    },
    GoalStoreCommitAck {
        goal_id: SurfaceGoalId,
        receipt: SurfaceGoalStoreReceipt,
    },
    OperationTerminalAck {
        thread_id: SurfaceThreadId,
        thread_owner_epoch: ThreadOwnerEpoch,
        operation_id: SurfaceOperationId,
        value: OperationTerminalAtCursor,
    },
    PolicyRevocationBarrierAck {
        plan: PolicyRevocationBarrierPlan,
        settled_owner_leases: Vec<UuidV7>,
        settled_resources: Vec<NonEmptyText>,
        proof: Sha256Digest,
    },
    HostCommitAck {
        host_incarnation: HostIncarnation,
        identity: HostReceiptIdentityPair,
        commit_id: SurfaceCommitId,
        receipt_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyRevocationSubject {
    OwnerLease(UuidV7),
    Resource(NonEmptyText),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownOperationSourcePhase {
    Requested,
    AdmittedReserved,
    AdmittedStarted,
    Suspended,
    BackgroundOwned,
    Finalizing,
    FinalizingDegraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownRequestCause {
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownSelectedCause {
    ExistingWinning { cause: OperationFinalizationCause },
    Requested { cause: ShutdownRequestCause },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownOperationPlan {
    ExistingTerminal {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        requirement: OperationTerminalAckRequirement,
    },
    PlannedFinalization {
        operation_id: SurfaceOperationId,
        source_phase: ShutdownOperationSourcePhase,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        selected_cause: ShutdownSelectedCause,
        expected_settlements: Vec<SurfaceSettlementId>,
        requirement: OperationTerminalAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EphemeralThreadPersistence {
    EphemeralNonCataloguedOneShot {
        close_after: FirstOperationCompletionPolicy,
    },
    EphemeralAttached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownThreadPlan {
    Recorded {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        operations: Vec<ShutdownOperationPlan>,
        session_closed: ThreadCursorAckRequirement,
        catalog_closed: HostReceiptAckRequirement,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        persistence: EphemeralThreadPersistence,
        operations: Vec<ShutdownOperationPlan>,
        session_closed: ThreadCursorAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownBarrierPlan {
    CloseThread {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        thread: ShutdownThreadPlan,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        plan_digest: Sha256Digest,
    },
    ShutdownHost {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        threads: Vec<ShutdownThreadPlan>,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        final_host_lifecycle: HostReceiptAckRequirement,
        plan_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownThreadRequirement {
    OperationTerminal(OperationTerminalAckRequirement),
    SessionClosed(ThreadCursorAckRequirement),
    CatalogClosed(HostReceiptAckRequirement),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownMissing {
    Thread {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
        requirement: ShutdownThreadRequirement,
    },
    HostLifecycle {
        requirement: HostReceiptAckRequirement,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownScope {
    CloseThread {
        thread_id: SurfaceThreadId,
        owner_epoch: ThreadOwnerEpoch,
    },
    ShutdownHost {
        host_incarnation: HostIncarnation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationDegradedState {
    pub settlement_id: SurfaceSettlementId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionDegradedState {
    pub durable_commit_id: SurfaceCommitId,
    pub fact_family: SurfaceFactFamily,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerAckPendingState {
    pub thread_owner_epoch: ThreadOwnerEpoch,
    pub durable_revision: DurableRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartCommitDegradedState {
    pub generation_fence: SurfaceOperationFence,
    pub started_commit_id: SurfaceCommitId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingFinalizationDeferredState {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub missing_settlements: NonEmptyVec<SurfaceSettlementId>,
    pub missing_set_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalProjectionDeferredState {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub terminal_event_id: SurfaceEventId,
    pub durable_revision: DurableRevision,
    pub terminal_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalizingDegradedState {
    MissingFinalization(MissingFinalizationDeferredState),
    TerminalProjectionPending(TerminalProjectionDeferredState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryPinPendingState {
    pub scope: MutationMemoryScope,
    pub record_id: SurfaceCatalogEntryId,
    pub memory_revision: MemoryRevision,
    pub thread_id: SurfaceThreadId,
    pub thread_owner_epoch: ThreadOwnerEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyRevocationPendingState {
    pub plan: PolicyRevocationBarrierPlan,
    pub pending: NonEmptyVec<PolicyRevocationSubject>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownDeferredState {
    pub plan: ShutdownBarrierPlan,
    pub missing: NonEmptyVec<ShutdownMissing>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeferredMutationState {
    MutationDegraded(MutationDegradedState),
    ProjectionDegraded(ProjectionDegradedState),
    OwnerAckPending(OwnerAckPendingState),
    StartCommitDegraded(StartCommitDegradedState),
    FinalizingDegraded(FinalizingDegradedState),
    MemoryPinPending(MemoryPinPendingState),
    PolicyRevocationPending(PolicyRevocationPendingState),
    ShutdownDeferred(ShutdownDeferredState),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceMutationErrorCode {
    InvalidRequest,
    InvalidInput,
    CommitBatchTooLarge,
    InvalidContent,
    UnsupportedContent,
    UnsupportedOperation,
    CapabilityDenied,
    WrongHost,
    WrongThread,
    WrongAttachment,
    WrongOwnerEpoch,
    UnknownOperation,
    UnknownGeneration,
    UnknownInteraction,
    UnknownTask,
    UnknownWorkflow,
    UnknownGoal,
    NoActiveGoal,
    UnknownSession,
    StaleFence,
    StaleRevision,
    StaleLease,
    StaleResponseRoute,
    WrongInteractionKind,
    WrongResponseToken,
    WrongAuthorityFingerprint,
    IllegalState,
    OperationAlreadyTerminal,
    OperationActive,
    OperationNotInterrupted,
    OperationNotSteerable,
    AdmissionClosed,
    CapacityExceeded,
    ThreadOwnedElsewhere,
    ThreadClosed,
    HostShuttingDown,
    CommitFailed,
    StoreUnavailable,
    RuntimeUnavailable,
    StalePublisherPermit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceMutationRevision {
    Thread {
        cursor: SurfaceCursor,
    },
    Host {
        host_incarnation: HostIncarnation,
        revision: HostRevisionWitness,
    },
    SessionCatalog {
        revision: SessionCatalogRevision,
    },
    McpCatalog {
        thread_id: SurfaceThreadId,
        revision: McpCatalogRevision,
    },
    InputCatalog {
        revision: InputCatalogRevision,
    },
    WorkflowCatalog {
        revision: WorkflowCatalogRevision,
    },
    SessionMetadata {
        thread_id: SurfaceThreadId,
        revision: SessionMetadataRevision,
    },
    Settings {
        host_incarnation: HostIncarnation,
        thread_id: Option<SurfaceThreadId>,
        revision: SettingsRevision,
    },
    Trust {
        canonical_path: CanonicalPath,
        revision: TrustRevision,
        policy_epoch: PolicyEpoch,
    },
    Memory {
        scope: MutationMemoryScope,
        revision: MemoryRevision,
    },
    ProjectRootMemory {
        root: CanonicalPath,
        revision: ProjectRootMemoryRevision,
    },
    Plan {
        thread_id: SurfaceThreadId,
        revision: PlanRevision,
    },
    Usage {
        thread_id: SurfaceThreadId,
        revision: UsageRevision,
    },
    Context {
        thread_id: SurfaceThreadId,
        revision: ContextRevision,
    },
    Goal {
        goal_id: SurfaceGoalId,
        revision: GoalRevision,
        owner_epoch: GoalOwnerEpoch,
    },
    Task {
        thread_id: SurfaceThreadId,
        revision: TaskRevision,
    },
    Workflow {
        thread_id: SurfaceThreadId,
        workflow_run_id: SurfaceWorkflowRunId,
        revision: WorkflowRevision,
    },
    Interaction {
        thread_id: SurfaceThreadId,
        interaction_id: SurfaceInteractionId,
        revision: InteractionRevision,
        route_epoch: ResponseRouteEpoch,
    },
    PinnedContext {
        thread_id: SurfaceThreadId,
        revision: PinnedContextRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceMutationError {
    pub code: SurfaceMutationErrorCode,
    pub message: DisplayText,
    pub winning_request_id: Option<SurfaceRequestId>,
    pub current_revision: Option<SurfaceMutationRevision>,
}

macro_rules! classified_mutation_error {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(SurfaceMutationError);

        #[allow(dead_code)]
        impl $name {
            pub(crate) const fn new(error: SurfaceMutationError) -> Self {
                Self(error)
            }

            pub fn error(&self) -> &SurfaceMutationError {
                &self.0
            }
        }
    };
}

classified_mutation_error!(InvalidMutationError);
classified_mutation_error!(StaleMutationError);
classified_mutation_error!(UnavailableMutationError);
classified_mutation_error!(CommitFailedMutationError);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedMutation {
    pub request_id: SurfaceRequestId,
    pub target: MutationTarget,
    pub disposition: MutationDisposition,
    pub acknowledgements: NonEmptyVec<MutationCommitAck>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileMutationToken {
    request_id: SurfaceRequestId,
    target: MutationTarget,
    settlement_id: SurfaceSettlementId,
    expected_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl ReconcileMutationToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            target,
            settlement_id,
            expected_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryStartCommitToken {
    request_id: SurfaceRequestId,
    thread_owner_epoch: ThreadOwnerEpoch,
    fence: SurfaceOperationFence,
    started_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl RetryStartCommitToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        thread_owner_epoch: ThreadOwnerEpoch,
        fence: SurfaceOperationFence,
        started_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            thread_owner_epoch,
            fence,
            started_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum RetryProjectionSelector {
    Local {
        fact_family: SurfaceFactFamily,
        event_id: SurfaceEventId,
    },
    Remote {
        durable_revision: DurableRevision,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryProjectionToken {
    request_id: SurfaceRequestId,
    target: MutationTarget,
    durable_commit_id: SurfaceCommitId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    selector: RetryProjectionSelector,
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryLocalProjectionToken(RetryProjectionToken);

#[derive(Clone, Eq, PartialEq)]
pub struct RetryRemoteProjectionToken(RetryProjectionToken);

#[allow(dead_code)]
impl RetryLocalProjectionToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        durable_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        fact_family: SurfaceFactFamily,
        event_id: SurfaceEventId,
    ) -> Self {
        Self(RetryProjectionToken {
            request_id,
            target,
            durable_commit_id,
            expected_thread_owner_epoch,
            selector: RetryProjectionSelector::Local {
                fact_family,
                event_id,
            },
        })
    }

    pub(crate) fn as_token(&self) -> RetryProjectionToken {
        self.0.clone()
    }
}

#[allow(dead_code)]
impl RetryRemoteProjectionToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        durable_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        durable_revision: DurableRevision,
    ) -> Self {
        Self(RetryProjectionToken {
            request_id,
            target,
            durable_commit_id,
            expected_thread_owner_epoch,
            selector: RetryProjectionSelector::Remote { durable_revision },
        })
    }

    pub(crate) fn as_token(&self) -> RetryProjectionToken {
        self.0.clone()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RetryFinalizationToken {
    request_id: SurfaceRequestId,
    thread_id: SurfaceThreadId,
    operation_id: SurfaceOperationId,
    finalize_intent_id: SurfaceFinalizeIntentId,
    terminal_commit_id: SurfaceCommitId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    missing_set_digest: Sha256Digest,
}

#[allow(dead_code)]
impl RetryFinalizationToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        terminal_commit_id: SurfaceCommitId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        missing_set_digest: Sha256Digest,
    ) -> Self {
        Self {
            request_id,
            thread_id,
            operation_id,
            finalize_intent_id,
            terminal_commit_id,
            expected_thread_owner_epoch,
            missing_set_digest,
        }
    }

    pub(crate) fn request_id(&self) -> &SurfaceRequestId {
        &self.request_id
    }

    pub(crate) fn thread_id(&self) -> &SurfaceThreadId {
        &self.thread_id
    }

    pub(crate) fn operation_id(&self) -> &SurfaceOperationId {
        &self.operation_id
    }
}

#[allow(dead_code)]
#[derive(Clone, Eq, PartialEq)]
enum ReconcileHostMutationTokenKind {
    Settlement {
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        host_incarnation: HostIncarnation,
        expected_commit_id: SurfaceCommitId,
    },
    Shutdown {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        scope: ShutdownScope,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        barrier_plan_digest: Sha256Digest,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileHostMutationToken(ReconcileHostMutationTokenKind);

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileHostSettlementToken(ReconcileHostMutationToken);

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileShutdownToken(ReconcileHostMutationToken);

#[allow(dead_code)]
impl ReconcileHostSettlementToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        target: MutationTarget,
        settlement_id: SurfaceSettlementId,
        host_incarnation: HostIncarnation,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self(ReconcileHostMutationToken(
            ReconcileHostMutationTokenKind::Settlement {
                request_id,
                target,
                settlement_id,
                host_incarnation,
                expected_commit_id,
            },
        ))
    }

    pub(crate) fn as_token(&self) -> ReconcileHostMutationToken {
        self.0.clone()
    }
}

#[allow(dead_code)]
impl ReconcileShutdownToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
        scope: ShutdownScope,
        barrier_id: SurfaceSettlementId,
        closing_commit_id: SurfaceCommitId,
        barrier_plan_digest: Sha256Digest,
    ) -> Self {
        Self(ReconcileHostMutationToken(
            ReconcileHostMutationTokenKind::Shutdown {
                request_id,
                host_incarnation,
                scope,
                barrier_id,
                closing_commit_id,
                barrier_plan_digest,
            },
        ))
    }

    pub(crate) fn as_token(&self) -> ReconcileHostMutationToken {
        self.0.clone()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileMemoryMutationToken {
    request_id: SurfaceRequestId,
    scope: MutationMemoryScope,
    memory_revision: MemoryRevision,
    record_id: SurfaceCatalogEntryId,
    pin_thread_id: SurfaceThreadId,
    expected_thread_owner_epoch: ThreadOwnerEpoch,
    expected_commit_id: SurfaceCommitId,
}

#[allow(dead_code)]
impl ReconcileMemoryMutationToken {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        scope: MutationMemoryScope,
        memory_revision: MemoryRevision,
        record_id: SurfaceCatalogEntryId,
        pin_thread_id: SurfaceThreadId,
        expected_thread_owner_epoch: ThreadOwnerEpoch,
        expected_commit_id: SurfaceCommitId,
    ) -> Self {
        Self {
            request_id,
            scope,
            memory_revision,
            record_id,
            pin_thread_id,
            expected_thread_owner_epoch,
            expected_commit_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconcileFolderTrustRevocationToken {
    request_id: SurfaceRequestId,
    expected_commit_id: SurfaceCommitId,
    plan: PolicyRevocationBarrierPlan,
}

#[allow(dead_code)]
impl ReconcileFolderTrustRevocationToken {
    pub(crate) fn new(
        request_id: SurfaceRequestId,
        expected_commit_id: SurfaceCommitId,
        plan: PolicyRevocationBarrierPlan,
    ) -> Self {
        Self {
            request_id,
            expected_commit_id,
            plan,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum DeferredRepair {
    ThreadMutation {
        state: MutationDegradedState,
        token: ReconcileMutationToken,
    },
    HostMutation {
        state: MutationDegradedState,
        token: ReconcileHostSettlementToken,
    },
    Projection {
        state: ProjectionDegradedState,
        token: RetryLocalProjectionToken,
    },
    TerminalProjection {
        state: TerminalProjectionDeferredState,
        token: RetryLocalProjectionToken,
    },
    RemoteOwner {
        state: OwnerAckPendingState,
        token: RetryRemoteProjectionToken,
    },
    Start {
        state: StartCommitDegradedState,
        token: RetryStartCommitToken,
    },
    Finalization {
        state: MissingFinalizationDeferredState,
        token: RetryFinalizationToken,
    },
    MemoryPin {
        state: MemoryPinPendingState,
        token: ReconcileMemoryMutationToken,
    },
    Policy {
        state: PolicyRevocationPendingState,
        token: ReconcileFolderTrustRevocationToken,
    },
    Shutdown {
        state: ShutdownDeferredState,
        token: ReconcileShutdownToken,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct DeferredMutation {
    pub request_id: SurfaceRequestId,
    pub target: MutationTarget,
    pub commit_id: SurfaceCommitId,
    pub committed_acknowledgements: Vec<MutationCommitAck>,
    pub missing_acknowledgements: NonEmptyVec<MutationAckRequirement>,
    pub repair: DeferredRepair,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UncommittedMutation {
    Invalid {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: InvalidMutationError,
    },
    Stale {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: StaleMutationError,
    },
    Unavailable {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: UnavailableMutationError,
    },
    CommitFailed {
        request_id: SurfaceRequestId,
        target: Option<MutationTarget>,
        error: CommitFailedMutationError,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum RuntimeSurfaceMutationResult {
    Committed(CommittedMutation),
    Deferred(DeferredMutation),
    Uncommitted(UncommittedMutation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeferredCommandValue<T> {
    NoValue,
    Provisional { value: T },
}

#[derive(Clone, Eq, PartialEq)]
pub enum MutationReply<T> {
    Committed {
        mutation: CommittedMutation,
        value: T,
    },
    Deferred {
        mutation: DeferredMutation,
        partial: DeferredCommandValue<T>,
    },
    Uncommitted {
        mutation: UncommittedMutation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedMutationReplay<T> {
    pub request_id: SurfaceRequestId,
    pub canonical_command_digest: Sha256Digest,
    pub target: MutationTarget,
    pub value: T,
    pub acknowledgements: NonEmptyVec<MutationCommitAck>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BackgroundTarget {
    ReservedOperation {
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    },
    ActiveGeneration {
        fence: SurfaceOperationFence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeSourceWitness {
    DurableReplay { replayability_digest: Sha256Digest },
    LiveCapsule { incarnation: SurfaceIncarnation },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceRecoverableOperation {
    operation_id: SurfaceOperationId,
    expected_last_generation: SurfaceGenerationId,
    resume_source: ResumeSourceWitness,
}

impl SurfaceRecoverableOperation {
    pub fn operation_id(&self) -> &SurfaceOperationId {
        &self.operation_id
    }
}

#[derive(Clone, PartialEq)]
pub enum InteractionSelector {
    Exact {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        kind: SurfaceInteractionKind,
        response_token: SurfaceResponseToken,
        response_route_epoch: ResponseRouteEpoch,
        response_grant_token: SurfaceResponseGrantToken,
        operation_fence: SurfaceOperationFence,
    },
    OpaqueRequestId {
        opaque_request_id: NonEmptyText,
        expected_kind: SurfaceInteractionKind,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum TaskControlAction {
    Stop { fence: SurfaceTaskFence },
    Foreground { fence: SurfaceTaskFence },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowControlAction {
    Launch {
        catalog_entry_id: SurfaceCatalogEntryId,
        observed_catalog_revision: WorkflowCatalogRevision,
        args: Vec<(NonEmptyText, DisplayText)>,
        parent: Option<SurfaceOperationFence>,
    },
    Pause {
        fence: SurfaceWorkflowFence,
    },
    Resume {
        fence: SurfaceWorkflowFence,
    },
    Stop {
        fence: SurfaceWorkflowFence,
    },
}

impl WorkflowControlAction {
    pub fn stop(workflow: &SurfaceWorkflow) -> Self {
        Self::Stop {
            fence: SurfaceWorkflowFence {
                workflow_run_id: workflow.workflow_run_id.clone(),
                workflow_revision: workflow.revision,
                parent: workflow.parent.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalRunInput {
    Supplied {
        request: SurfaceInputRequest,
    },
    DerivedFromGoal {
        goal_id: SurfaceGoalId,
        objective_revision: GoalObjectiveRevision,
        goal_receipt_digest: Sha256Digest,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum ExpectedGoal {
    None,
    Exact(SurfaceGoalFence),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTokenBudgetUpdate {
    Keep,
    Set(Option<i64>),
}

#[derive(Clone, Eq, PartialEq)]
pub enum GoalMutationAction {
    SetAndRun {
        expected_goal: ExpectedGoal,
        objective: NonEmptyText,
        token_budget: Option<i64>,
        input: GoalRunInput,
    },
    Edit {
        fence: SurfaceGoalFence,
        objective: NonEmptyText,
        token_budget: GoalTokenBudgetUpdate,
    },
    Clear {
        fence: SurfaceGoalFence,
    },
    ResumeAndRun {
        fence: SurfaceGoalFence,
        input: GoalRunInput,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PinnedContextAction {
    Add {
        expected_revision: PinnedContextRevision,
        entry: SurfacePinnedContextEntry,
        memory_receipt: Option<(SurfaceCatalogEntryId, MemoryRevision)>,
    },
    Remove {
        expected_revision: PinnedContextRevision,
        entry_id: SurfaceCatalogEntryId,
    },
    Clear {
        expected_revision: PinnedContextRevision,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpCatalogFamily {
    Tools,
    Resources,
    ResourceTemplates,
}

#[derive(Clone, Eq, PartialEq)]
pub struct McpCatalogCursor {
    pub thread_id: SurfaceThreadId,
    pub revision: McpCatalogRevision,
    pub family: McpCatalogFamily,
    pub offset: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum McpCatalogQuery {
    ListTools {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    ListResources {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    ListResourceTemplates {
        cursor: Option<McpCatalogCursor>,
        limit: u32,
    },
    Lookup {
        id: SurfaceCatalogEntryId,
    },
}

pub enum SurfaceCommand {
    ReserveOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        intent: OperationRequestIntent,
    },
    AdmitReserved {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
        admission_lease_id: SurfaceAdmissionLeaseId,
    },
    CancelOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
    },
    CancelSessionCurrent {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        legacy_rpc_id_digest: Sha256Digest,
    },
    InterruptGeneration {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        fence: SurfaceOperationFence,
    },
    PauseGoalOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        goal_fence: SurfaceGoalFence,
    },
    ResumeOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        operation_id: SurfaceOperationId,
        expected_last_generation: SurfaceGenerationId,
        resume_source: ResumeSourceWitness,
    },
    SteerOperation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        fence: SurfaceOperationFence,
        input: SurfaceInputRequest,
    },
    TransferBackground {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        target: BackgroundTarget,
    },
    RespondInteraction {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        selector: InteractionSelector,
        response: BoundInteractionResponse,
    },
    ReconcileMutation {
        token: ReconcileMutationToken,
    },
    RetryStartCommit {
        token: RetryStartCommitToken,
    },
    RetryProjection {
        token: RetryProjectionToken,
    },
    RetryFinalization {
        token: RetryFinalizationToken,
    },
    ManualCompact {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_context_revision: ContextRevision,
    },
    Backtrack {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_cursor: SurfaceCursor,
        target: LastUserTurn,
    },
    TaskControl {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: TaskControlAction,
    },
    WorkflowControl {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: WorkflowControlAction,
    },
    GoalMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: GoalMutationAction,
    },
    SettingsMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceHostBoundCaller,
        host_incarnation: HostIncarnation,
        expected_thread_revision: SettingsRevision,
        patch: RuntimeSettingsPatch,
    },
    McpCatalogQuery {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        expected_revision: Option<McpCatalogRevision>,
        query: McpCatalogQuery,
    },
    PinnedContextMutation {
        request_id: SurfaceRequestId,
        caller: SurfaceBoundCaller,
        action: PinnedContextAction,
    },
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct OperationWaiterHandle(Arc<()>);

#[allow(dead_code)]
impl OperationWaiterHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}

#[derive(Clone)]
pub struct ReservedOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub lease: ReservationLease,
    pub requested_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone)]
pub enum AdmissionOutput {
    Queued {
        operation_id: SurfaceOperationId,
        queue_position: u32,
        lease: ReservationLease,
        waiter: OperationWaiterHandle,
    },
    Admitted {
        operation_id: SurfaceOperationId,
        first_generation: SurfaceOperationFence,
        admitted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub enum CancelOperationOutput {
    CancelledBeforeAdmission {
        terminal: OperationTerminalAtCursor,
    },
    Accepted {
        operation_id: SurfaceOperationId,
        accepted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
    AlreadyTerminal {
        terminal: OperationTerminalAtCursor,
    },
    FinalizationPending {
        operation_id: SurfaceOperationId,
        finalize_intent_id: SurfaceFinalizeIntentId,
        finalization_cursor: FinalizationStartedAtCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub enum CancelSessionCurrentResult {
    NoCurrentOperation {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
    },
    Resolved {
        mutation: MutationReply<CancelOperationOutput>,
    },
}

#[derive(Clone)]
pub struct InterruptOutput {
    pub fence: SurfaceOperationFence,
    pub accepted_cursor: SurfaceCursor,
    pub settlement: InterruptSettlement,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone)]
pub enum PauseGoalOperationOutput {
    None,
    CancelledBeforeAdmission {
        terminal: OperationTerminalAtCursor,
    },
    Cancelling {
        operation_id: SurfaceOperationId,
        accepted_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone)]
pub struct PauseGoalOutput {
    pub goal: SurfaceGoal,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub goal_cursor: SurfaceCursor,
    pub operation: PauseGoalOperationOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeTransitionRole {
    ResumeStarting,
    GenerationReserved,
    GenerationStarted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeTransitionReceipt {
    pub role: ResumeTransitionRole,
    pub event_id: SurfaceEventId,
    pub cursor: SurfaceCursor,
    pub commit_class: CommitClass,
}

#[derive(Clone)]
pub struct ResumeOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub generation: SurfaceOperationFence,
    pub resume_starting: ResumeTransitionReceipt,
    pub generation_reserved: ResumeTransitionReceipt,
    pub generation_started: ResumeTransitionReceipt,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SteerOutput {
    pub fence: SurfaceOperationFence,
    pub input_item_id: SurfaceItemId,
    pub committed_cursor: SurfaceCursor,
}

#[derive(Clone)]
pub enum TransferBackgroundOutput {
    QueuedOnStart {
        operation_id: SurfaceOperationId,
        intent_cursor: SurfaceCursor,
    },
    HandedOff {
        background_fence: SurfaceBackgroundFence,
        handoff_cursor: SurfaceCursor,
        waiter: OperationWaiterHandle,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RespondInteractionDisposition {
    Resolved {
        receipt: SurfaceInteractionResolutionReceipt,
    },
    AlreadyResolved {
        winning_receipt: SurfaceInteractionResolutionReceipt,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RespondInteractionOutput {
    pub interaction_id: SurfaceInteractionId,
    pub attempted_response_id: SurfaceResponseId,
    pub disposition: RespondInteractionDisposition,
    pub projected_cursor: Option<SurfaceCursor>,
}

#[derive(Clone)]
pub struct MaintenanceOperationOutput {
    pub operation_id: SurfaceOperationId,
    pub admitted_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
}

#[derive(Clone, PartialEq)]
pub struct TaskControlOutput {
    pub task: SurfaceTask,
    pub cursor: SurfaceCursor,
}

#[derive(Clone)]
pub struct WorkflowControlOutput {
    pub workflow: SurfaceWorkflow,
    pub operation_id: Option<SurfaceOperationId>,
    pub cursor: SurfaceCursor,
    pub waiter: Option<OperationWaiterHandle>,
}

#[derive(Clone)]
pub struct GoalMutationOutput {
    pub goal: Option<SurfaceGoal>,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub change_cursor: SurfaceCursor,
    pub operation_id: Option<SurfaceOperationId>,
    pub waiter: Option<OperationWaiterHandle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsMutationOutput {
    pub settings: SurfaceSettingsSnapshot,
    pub cursor: SurfaceCursor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceCatalogEntry {
    McpTool(SurfaceMcpTool),
    McpResource(SurfaceMcpResource),
    McpResourceTemplate(SurfaceMcpResourceTemplate),
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpCatalogPageValues {
    Tools(Vec<SurfaceMcpTool>),
    Resources(Vec<SurfaceMcpResource>),
    ResourceTemplates(Vec<SurfaceMcpResourceTemplate>),
    Entry(SurfaceCatalogEntry),
}

#[derive(Clone, PartialEq)]
pub struct McpCatalogPage {
    pub revision: McpCatalogRevision,
    pub values: McpCatalogPageValues,
    pub next_cursor: Option<McpCatalogCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedContextMutationOutput {
    pub snapshot: SurfacePinnedContextSnapshot,
    pub cursor: SurfaceCursor,
}

/// Bounded, user-visible content from the latest durable child checkpoint.
///
/// This is deliberately a separate projection from [`SurfaceHistoryMessage`].
/// The latter is a compatibility view and can contain provider reasoning,
/// raw tool arguments, and filesystem metadata.  A task transcript query must
/// never make those private fields available to a surface client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTranscriptSnapshot {
    /// The task whose checkpoint was read.
    pub task_id: SurfaceTaskId,
    /// Surface task revision used to fence the task lookup.
    pub task_revision: TaskRevision,
    /// CAS revision of the continuation record that supplied this checkpoint.
    pub checkpoint_revision: u64,
    /// Number of completed child turns represented by the checkpoint.
    pub turn: u32,
    /// Cumulative child budget usage at the checkpoint.
    pub usage: BudgetUsage,
    /// Whether the continuation has reached a durable terminal state.
    pub complete: bool,
    /// User-visible conversation and tool boundary items.
    pub items: Vec<TaskTranscriptItem>,
}

/// Safe transcript item projection.  Hidden reasoning, raw tool arguments,
/// image payloads, continuation paths, and working-directory metadata are
/// intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskTranscriptItem {
    User {
        content: DisplayText,
    },
    Assistant {
        content: DisplayText,
    },
    ToolCall {
        id: SurfaceHistoryId,
        name: NonEmptyText,
    },
    ToolResult {
        id: SurfaceHistoryId,
        content: DisplayText,
        status: TaskTranscriptToolStatus,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskTranscriptToolStatus {
    Completed,
    Failed,
    Denied,
    NotImplemented,
    Cancelled,
    Indeterminate,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceReadRevision {
    Host {
        host_incarnation: HostIncarnation,
        revision: HostRevisionWitness,
    },
    SessionCatalog {
        revision: SessionCatalogRevision,
    },
    McpCatalog {
        thread_id: SurfaceThreadId,
        revision: McpCatalogRevision,
    },
    InputCatalog {
        revision: InputCatalogRevision,
    },
    WorkflowCatalog {
        revision: WorkflowCatalogRevision,
    },
    Thread {
        cursor: SurfaceCursor,
    },
    Session {
        token: SessionReadToken,
    },
    Task {
        task_id: SurfaceTaskId,
        revision: TaskRevision,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReadErrorCode {
    InvalidRequest,
    InvalidCursor,
    CapabilityDenied,
    NotFound,
    StaleRevision,
    BindingMismatch,
    ThreadOwnedElsewhere,
    ThreadClosed,
    StoreUnavailable,
    RuntimeUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceReadErrorClass {
    NotFound,
    Invalid,
    Stale,
    Unavailable,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceReadError {
    pub class: SurfaceReadErrorClass,
    pub code: SurfaceReadErrorCode,
    pub message: DisplayText,
    pub current_revision: Option<SurfaceReadRevision>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceReadResult<T> {
    Found {
        request_id: SurfaceRequestId,
        revision: SurfaceReadRevision,
        value: T,
    },
    NotFound {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Invalid {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Stale {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
    Unavailable {
        request_id: SurfaceRequestId,
        error: SurfaceReadError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSortKey {
    CreatedAt,
    UpdatedAt,
    RecencyAt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionListArchiveFilter {
    ActiveOnly,
    ArchivedOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSearchArchiveFilter {
    ActiveOnly,
    ActiveAndArchived,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRelationFilter {
    DirectChildrenOf { parent_thread_id: SurfaceThreadId },
    DescendantsOf { ancestor_thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionSetFilter<T: Ord> {
    Any,
    Match(NonEmptySet<T>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfacePageLimit {
    ClientBounded {
        value: u32,
    },
    LegacyJsonl {
        wire_value: u64,
        effective: NonZeroU64,
    },
}

impl SurfacePageLimit {
    pub fn try_session_catalog(value: u32) -> Result<Self, SurfaceValueError> {
        Self::try_client_bounded(value, 100)
    }

    pub fn try_thread_page(value: u32) -> Result<Self, SurfaceValueError> {
        Self::try_client_bounded(value, 500)
    }

    pub fn legacy_jsonl(wire_value: u64) -> Self {
        Self::LegacyJsonl {
            wire_value,
            effective: NonZeroU64::new(wire_value).unwrap_or(NonZeroU64::MIN),
        }
    }

    fn try_client_bounded(value: u32, maximum: u32) -> Result<Self, SurfaceValueError> {
        if value == 0 {
            return Err(SurfaceValueError::Zero);
        }
        if value > maximum {
            return Err(SurfaceValueError::TooLong {
                maximum: maximum as usize,
                observed: value as usize,
            });
        }
        Ok(Self::ClientBounded { value })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListFilter {
    pub cwd: Vec<CanonicalPath>,
    pub providers: SessionSetFilter<NonEmptyText>,
    pub models: SessionSetFilter<NonEmptyText>,
    pub relation: Option<SessionRelationFilter>,
    pub archived: SessionListArchiveFilter,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionCatalogCursor {
    pub catalog_revision: SessionCatalogRevision,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub query_digest: Sha256Digest,
    pub last_value_digest: Sha256Digest,
    pub last_thread_id: SurfaceThreadId,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyJsonlPageCursor {
    pub wire_value: DisplayText,
    pub effective_offset: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceSessionPageCursor {
    Typed(SessionCatalogCursor),
    LegacyJsonl(LegacyJsonlPageCursor),
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionPageRequest {
    pub filters: SessionListFilter,
    pub search_term: Option<NonEmptyText>,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub cursor: Option<SurfaceSessionPageCursor>,
    pub limit: SurfacePageLimit,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionSearchRequest {
    pub query: NonEmptyText,
    pub archived: SessionSearchArchiveFilter,
    pub sort_key: SessionSortKey,
    pub direction: SortDirection,
    pub cursor: Option<SurfaceSessionPageCursor>,
    pub limit: SurfacePageLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionSummary {
    pub thread_id: SurfaceThreadId,
    pub title: DisplayText,
    pub cwd: CanonicalPath,
    pub provider: NonEmptyText,
    pub model: Option<NonEmptyText>,
    pub created_at: Rfc3339Timestamp,
    pub updated_at: Rfc3339Timestamp,
    pub parent_thread_id: Option<SurfaceThreadId>,
    pub forked: bool,
    pub archived: bool,
    pub approval_mode: Option<SurfaceApprovalMode>,
    pub active_permission_profile: Option<SurfaceActivePermissionProfile>,
    pub permission_rule_count: u64,
    pub runtime_workspace_roots: Vec<CanonicalPath>,
    pub additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
    pub network_permissions: SurfaceNetworkPermissions,
    pub message_count: u64,
    pub turn_count: u64,
    pub metadata_revision: SessionMetadataRevision,
    pub running: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceSessionSummaryPage {
    pub catalog_revision: SessionCatalogRevision,
    pub data: Vec<SurfaceSessionSummary>,
    pub next_cursor: Option<SurfaceSessionPageCursor>,
    pub backwards_cursor: Option<SurfaceSessionPageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionSearchHit {
    pub thread: SurfaceSessionSummary,
    pub snippet: DisplayText,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceSessionSearchPage {
    pub catalog_revision: SessionCatalogRevision,
    pub data: Vec<SurfaceSessionSearchHit>,
    pub next_cursor: Option<SurfaceSessionPageCursor>,
    pub backwards_cursor: Option<SurfaceSessionPageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionReadToken {
    pub thread_id: SurfaceThreadId,
    pub durable_revision: DurableRevision,
    pub metadata_revision: SessionMetadataRevision,
    pub snapshot_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionMetadata {
    pub summary: SurfaceSessionSummary,
    pub runtime_workspace_roots: Vec<CanonicalPath>,
    pub active_permission_profile: Option<SurfaceActivePermissionProfile>,
    pub permission_rules: SurfacePermissionRuleSet,
    pub additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
    pub network_permissions: SurfaceNetworkPermissions,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceHistoryId(NonEmptyText);

impl SurfaceHistoryId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SurfaceValueError> {
        NonEmptyText::try_new(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistorySystemRole {
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryUserRole {
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryAssistantRole {
    Assistant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryToolRole {
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryStatus {
    InProgressSnakeCase,
    InProgressCamelCase,
    Running,
    Completed,
    Failed,
    NotImplementedSnakeCase,
    Cancelled,
    Indeterminate,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryTerminalStatus {
    Completed,
    Failed,
    NotImplementedSnakeCase,
    Cancelled,
    Indeterminate,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryRunningStatus {
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHistoryToolKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
    Cancelled,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceHistoryMessage {
    System {
        role: SurfaceHistorySystemRole,
        content: DisplayText,
    },
    User {
        role: SurfaceHistoryUserRole,
        content: DisplayText,
    },
    Assistant {
        role: SurfaceHistoryAssistantRole,
        content: Option<DisplayText>,
        reasoning_content: Option<DisplayText>,
        tool_calls: Vec<SurfaceDataValue>,
    },
    Tool {
        role: SurfaceHistoryToolRole,
        tool_call_id: SurfaceHistoryId,
        content: DisplayText,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChangeKind {
    Edit,
    Write,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryFileChange {
    pub path: Option<DisplayText>,
    pub kind: FileChangeKind,
    pub diff: SurfaceDataValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceHistoryItem {
    PersistedMessage {
        message: SurfaceHistoryMessage,
    },
    UserMessage {
        content: DisplayText,
    },
    AgentMessage {
        id: SurfaceHistoryId,
        text: DisplayText,
    },
    Plan {
        id: SurfaceHistoryId,
        text: DisplayText,
    },
    Reasoning {
        id: SurfaceHistoryId,
        summary: DisplayText,
        content: DisplayText,
    },
    CommandExecution {
        id: SurfaceHistoryId,
        tool: NonEmptyText,
        command: Option<DisplayText>,
        cwd: Option<CanonicalPath>,
        process_id: Option<SurfaceHistoryId>,
        source: Option<NonEmptyText>,
        status: SurfaceHistoryStatus,
        command_actions: Vec<SurfaceDataValue>,
        aggregated_output: Option<DisplayText>,
        error: Option<SurfaceDataValue>,
        exit_code: Option<i32>,
        truncated: Option<bool>,
        duration_ms: Option<u64>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    ToolResult {
        tool_call_id: SurfaceHistoryId,
        content: DisplayText,
        status: Option<SurfaceHistoryStatus>,
        error: Option<SurfaceDataValue>,
        exit_code: Option<i32>,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    McpToolCall {
        id: SurfaceHistoryId,
        server: NonEmptyText,
        tool: NonEmptyText,
        status: SurfaceHistoryStatus,
        arguments: SurfaceDataValue,
        result: SurfaceDataValue,
        error: SurfaceDataValue,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    DynamicToolCall {
        id: SurfaceHistoryId,
        namespace: Option<NonEmptyText>,
        tool: NonEmptyText,
        status: SurfaceHistoryStatus,
        arguments: SurfaceDataValue,
        content_items: Option<Vec<SurfaceDataValue>>,
        success: Option<bool>,
        error: Option<SurfaceDataValue>,
        truncated: Option<bool>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    FileChange {
        id: SurfaceHistoryId,
        status: SurfaceHistoryStatus,
        changes: NonEmptyVec<SurfaceHistoryFileChange>,
        error: Option<SurfaceDataValue>,
        kind: Option<SurfaceHistoryToolKind>,
        terminal_source: Option<ToolTerminalSource>,
        invocation_started: Option<ToolInvocationStarted>,
    },
    WorkflowStarted {
        id: SurfaceHistoryId,
        workflow_name: NonEmptyText,
        task_id: SurfaceHistoryId,
        status: SurfaceHistoryRunningStatus,
        task: SurfaceDataValue,
    },
    WorkflowTerminal {
        id: SurfaceHistoryId,
        workflow_name: NonEmptyText,
        task_id: SurfaceHistoryId,
        status: SurfaceHistoryTerminalStatus,
        result: SurfaceDataValue,
        error: SurfaceDataValue,
        task: SurfaceDataValue,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadItemTurnFilter {
    Any,
    Exact(SurfaceHistoryId),
    MatchNone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadPageQuery {
    Messages {
        direction: SortDirection,
    },
    Turns {
        direction: SortDirection,
        items_view: TurnItemsView,
    },
    Items {
        turn: ThreadItemTurnFilter,
        direction: SortDirection,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ThreadPageCursor {
    pub read_token: SessionReadToken,
    pub query_digest: Sha256Digest,
    pub next_ordinal: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceThreadPageCursor {
    Typed(ThreadPageCursor),
    LegacyJsonl(LegacyJsonlPageCursor),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryTurn {
    pub thread_id: SurfaceHistoryId,
    pub turn_id: SurfaceHistoryId,
    pub index: u64,
    pub role: SurfaceHistoryRole,
    pub items_view: TurnItemsView,
    pub items: Vec<SurfaceHistoryItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHistoryItemEntry {
    pub thread_id: SurfaceHistoryId,
    pub turn_id: SurfaceHistoryId,
    pub item_id: SurfaceHistoryId,
    pub index: u64,
    pub item: SurfaceHistoryItem,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceThreadPage {
    Messages {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryMessage>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
    Turns {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryTurn>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
    Items {
        read_token: SessionReadToken,
        data: Vec<SurfaceHistoryItemEntry>,
        next_cursor: Option<SurfaceThreadPageCursor>,
        backwards_cursor: Option<SurfaceThreadPageCursor>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSessionReadBundle {
    pub metadata: SurfaceSessionMetadata,
    pub read_token: SessionReadToken,
    pub messages: Vec<SurfaceHistoryMessage>,
    pub turns: Vec<SurfaceHistoryTurn>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadSessionMetadataOutput {
    pub metadata: SurfaceSessionMetadata,
    pub read_token: SessionReadToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretReference {
    Environment { name: NonEmptyText },
    HostSecretStore { key: NonEmptyText },
}

pub enum SurfaceMcpValue {
    LiteralNonSecret { value: DisplayText },
    Secret { reference: SecretReference },
    EphemeralSecret { value: ZeroizingProcessLocalSecret },
}

pub enum SurfaceMcpTransport {
    Stdio {
        command: NonEmptyText,
        args: Vec<SurfaceMcpValue>,
        env: Vec<(NonEmptyText, SurfaceMcpValue)>,
    },
    Sse {
        url: CanonicalUri,
        headers: Vec<(NonEmptyText, SurfaceMcpValue)>,
    },
}

pub struct SurfaceMcpServerDeclaration {
    pub name: NonEmptyText,
    pub transport: SurfaceMcpTransport,
    pub startup_timeout: DurationMillis,
    pub tool_timeout: DurationMillis,
    pub disabled: bool,
}

pub struct SurfaceThreadCreateSpec {
    pub title: DisplayText,
    pub persistence: ThreadPersistence,
    pub settings_overrides: Vec<RuntimeSettingsPatch>,
    pub mcp_servers: Vec<SurfaceMcpServerDeclaration>,
    pub parent_thread_id: Option<SurfaceThreadId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenThreadMode {
    LiveOnly,
    LiveOrMaterialize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveOnly {
    LiveOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionMetadataPrecondition {
    Exact { revision: SessionMetadataRevision },
    LegacyLastWriteWins,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionMetadataPatch {
    SetTitle { title: DisplayText },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryScope {
    User {
        expected_memory_revision: Option<MemoryRevision>,
    },
    Project {
        canonical_root: CanonicalPath,
        expected_root_revision: ProjectRootMemoryRevision,
        expected_memory_revision: Option<MemoryRevision>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSettingsTarget {
    HostDefaults,
    Thread { thread_id: SurfaceThreadId },
    HostDefaultsAndThread { thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsExpectedRevision {
    pub host: SettingsRevision,
    pub thread: Option<SettingsRevision>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct InputCatalogCursor {
    pub revision: InputCatalogRevision,
    pub context_digest: Sha256Digest,
    pub query_digest: Sha256Digest,
    pub offset: u64,
    pub cursor_authenticator: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum InputCatalogQuery {
    Search {
        query: DisplayText,
        kinds: Set<SurfaceInputBindingKind>,
        cursor: Option<InputCatalogCursor>,
        limit: u32,
    },
    Lookup {
        id: SurfaceCatalogEntryId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputCatalogContext {
    HostDefaults {
        host_incarnation: HostIncarnation,
        settings_revision: SettingsRevision,
    },
    Thread {
        thread_id: SurfaceThreadId,
        settings_revision: SettingsRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonlTurnControlAction {
    Interrupt,
    Resume,
    Steer { input: SurfaceInputRequest },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlResolvedTurnControlStatus {
    Interrupted,
    Resumed,
    Steered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlTurnControlWireAction {
    Interrupt,
    Resume,
    Steer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlIdleTurnControlStatus {
    Idle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlResolvedTurnControlWireEcho {
    pub legacy_turn_id: LegacyTurnId,
    pub action: JsonlTurnControlWireAction,
    pub status: JsonlResolvedTurnControlStatus,
    pub legacy_input: Option<DisplayText>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlIdleTurnControlWireEcho {
    pub legacy_turn_id: LegacyTurnId,
    pub action: JsonlTurnControlWireAction,
    pub status: JsonlIdleTurnControlStatus,
    pub legacy_input: Option<DisplayText>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlTurnControlledOutput {
    pub operation_id: SurfaceOperationId,
    pub echo: JsonlResolvedTurnControlWireEcho,
    pub committed_cursor: SurfaceCursor,
    pub input_item_id: Option<SurfaceItemId>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum JsonlTurnControlResult {
    Idle {
        request_id: SurfaceRequestId,
        echo: JsonlIdleTurnControlWireEcho,
    },
    Resolved {
        mutation: MutationReply<JsonlTurnControlledOutput>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceInputCatalogEntry {
    pub id: SurfaceCatalogEntryId,
    pub kind: SurfaceInputBindingKind,
    pub label: NonEmptyText,
    pub description: Option<DisplayText>,
    pub catalog_revision: InputCatalogRevision,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SurfaceInputCatalogPage {
    pub revision: InputCatalogRevision,
    pub data: Vec<SurfaceInputCatalogEntry>,
    pub next_cursor: Option<InputCatalogCursor>,
}

pub enum SurfaceHostCommand {
    ListSessions {
        request_id: SurfaceRequestId,
        page: SessionPageRequest,
    },
    SearchSessions {
        request_id: SurfaceRequestId,
        search: SessionSearchRequest,
    },
    ReadSessionMetadata {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
    },
    ReadSession {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        include_messages: bool,
        include_turns: bool,
    },
    ReadThreadPage {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        query: ThreadPageQuery,
        read_token: Option<SessionReadToken>,
        cursor: Option<SurfaceThreadPageCursor>,
        limit: SurfacePageLimit,
    },
    CreateThread {
        request_id: SurfaceRequestId,
        spec: SurfaceThreadCreateSpec,
    },
    OpenThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        mode: OpenThreadMode,
        expected_settings_digest: Option<Sha256Digest>,
    },
    LoadThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        expected_settings_digest: Option<Sha256Digest>,
        settings_overrides: Vec<RuntimeSettingsPatch>,
        mcp_servers: Vec<SurfaceMcpServerDeclaration>,
    },
    ForkThread {
        request_id: SurfaceRequestId,
        source_thread_id: SurfaceThreadId,
        source_read_token: SessionReadToken,
        title: Option<DisplayText>,
        settings_overrides: Vec<RuntimeSettingsPatch>,
    },
    ResolveRunningThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        mode: LiveOnly,
    },
    ResumeLatestActiveGoal {
        request_id: SurfaceRequestId,
        expected_goal_store_revision: Option<GoalCatalogRevision>,
    },
    UpdateSessionMetadata {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        precondition: SessionMetadataPrecondition,
        patch: SessionMetadataPatch,
    },
    QueryInputCatalog {
        request_id: SurfaceRequestId,
        context: InputCatalogContext,
        expected_revision: Option<InputCatalogRevision>,
        query: InputCatalogQuery,
    },
    ControlJsonlTurn {
        request_id: SurfaceRequestId,
        expected_thread_id: Option<SurfaceThreadId>,
        legacy_turn_id: LegacyTurnId,
        action: JsonlTurnControlAction,
    },
    RememberMemory {
        request_id: SurfaceRequestId,
        scope: MemoryScope,
        note: NonEmptyText,
        pin_to_thread: Option<SurfaceThreadId>,
    },
    ReconcileMemoryMutation {
        token: ReconcileMemoryMutationToken,
    },
    ReadFolderTrust {
        request_id: SurfaceRequestId,
        path: CanonicalPath,
    },
    SetFolderTrust {
        request_id: SurfaceRequestId,
        path: CanonicalPath,
        expected_trust_revision: TrustRevision,
        level: FolderTrustLevel,
    },
    ReconcileFolderTrustRevocation {
        token: ReconcileFolderTrustRevocationToken,
    },
    ReadRuntimeSettings {
        request_id: SurfaceRequestId,
        thread_id: Option<SurfaceThreadId>,
    },
    UpdateRuntimeSettings {
        request_id: SurfaceRequestId,
        target: RuntimeSettingsTarget,
        expected: RuntimeSettingsExpectedRevision,
        patch: NonEmptyVec<RuntimeSettingsPatch>,
    },
    ReconcileHostMutation {
        token: ReconcileHostMutationToken,
    },
    CloseThread {
        request_id: SurfaceRequestId,
        thread_id: SurfaceThreadId,
        expected_owner_epoch: Option<ThreadOwnerEpoch>,
    },
    ShutdownHost {
        request_id: SurfaceRequestId,
        host_incarnation: HostIncarnation,
    },
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceHostHandle {
    host_incarnation: HostIncarnation,
    grant: NonEmptySet<SurfaceCapability>,
    connection_id: Option<SurfaceConnectionId>,
    pub(crate) runtime: Option<crate::runtime_host::RuntimeHostHandle>,
}

#[allow(dead_code)]
impl RuntimeSurfaceHostHandle {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        grant: NonEmptySet<SurfaceCapability>,
        connection_id: Option<SurfaceConnectionId>,
    ) -> Self {
        Self {
            host_incarnation,
            grant,
            connection_id,
            runtime: None,
        }
    }

    pub(crate) fn from_runtime(runtime: crate::runtime_host::RuntimeHostHandle) -> Self {
        Self {
            host_incarnation: runtime.host_incarnation().clone(),
            grant: NonEmptySet::try_new(BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::SubmitOperation,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::ManageThreadSettings,
                SurfaceCapability::RespondGrantedInteraction,
                SurfaceCapability::RepairThread,
            ]))
            .expect("runtime surface host grant is non-empty"),
            connection_id: None,
            runtime: Some(runtime),
        }
    }

    pub(crate) fn bind_new_connection(&self) -> Self {
        let mut bound = self.clone();
        bound.connection_id = Some(
            SurfaceConnectionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated ACP connection id is valid"),
        );
        bound
    }

    pub(crate) fn connection_id(&self) -> Option<&SurfaceConnectionId> {
        self.connection_id.as_ref()
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct RuntimeSurfaceHandle {
    host_incarnation: HostIncarnation,
    thread_id: SurfaceThreadId,
    authority: SurfaceAttachAuthority,
    hub: Option<SurfaceHub>,
}

#[allow(dead_code)]
impl RuntimeSurfaceHandle {
    pub(crate) fn new(
        host_incarnation: HostIncarnation,
        thread_id: SurfaceThreadId,
        authority: SurfaceAttachAuthority,
    ) -> Self {
        debug_assert_eq!(&host_incarnation, authority.host_incarnation());
        debug_assert_eq!(&thread_id, authority.thread_id());
        Self {
            host_incarnation,
            thread_id,
            authority,
            hub: None,
        }
    }

    pub(crate) fn from_hub(hub: SurfaceHub) -> Self {
        let authority = hub.authority().clone();
        Self {
            host_incarnation: authority.host_incarnation().clone(),
            thread_id: authority.thread_id().clone(),
            authority,
            hub: Some(hub),
        }
    }

    pub fn attach_fresh(&self, request: FreshAttachRequest) -> AttachResult {
        self.hub.as_ref().map_or(
            AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::RuntimeUnavailable,
            },
            |hub| hub.attach_fresh(request),
        )
    }

    pub(crate) fn attach_acp_fresh(
        &self,
        request: FreshAttachRequest,
        capability_profile: AcpAttachmentCapabilityProfile,
    ) -> AttachResult {
        self.hub.as_ref().map_or(
            AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::RuntimeUnavailable,
            },
            |hub| hub.attach_acp_fresh(request, capability_profile),
        )
    }

    pub fn claim_subscription(
        &self,
        handle: &SurfaceSubscriptionHandle,
    ) -> Option<SurfaceSubscriptionReceiver> {
        self.hub.as_ref()?.claim_subscription(handle)
    }

    pub(crate) fn claim_acp_read_text_file_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpReadTextFileDispatchReceiver> {
        self.hub.as_ref()?.claim_acp_read_text_file_dispatch(client)
    }

    pub(crate) fn claim_acp_write_text_file_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpWriteTextFileDispatchReceiver> {
        self.hub
            .as_ref()?
            .claim_acp_write_text_file_dispatch(client)
    }

    pub(crate) fn claim_acp_terminal_create_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalCreateDispatchReceiver> {
        self.hub
            .as_ref()?
            .claim_acp_terminal_create_dispatch(client)
    }

    pub(crate) fn claim_acp_terminal_observation_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalObservationDispatchReceiver> {
        self.hub
            .as_ref()?
            .claim_acp_terminal_observation_dispatch(client)
    }

    pub(crate) fn claim_acp_terminal_cleanup_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalCleanupDispatchReceiver> {
        self.hub
            .as_ref()?
            .claim_acp_terminal_cleanup_dispatch(client)
    }

    pub fn detach(
        &self,
        client: &RuntimeSurfaceClientHandle,
        request: DetachRequest,
    ) -> DetachResult {
        match self.hub.as_ref() {
            Some(hub) => hub.detach(client, request),
            None => DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id: client.attachment_id().clone(),
            },
        }
    }

    pub(crate) fn host_incarnation(&self) -> &HostIncarnation {
        &self.host_incarnation
    }

    pub(crate) fn thread_id(&self) -> &SurfaceThreadId {
        &self.thread_id
    }

    pub(crate) fn authority(&self) -> &SurfaceAttachAuthority {
        &self.authority
    }

    pub(crate) fn with_authority(&self, authority: SurfaceAttachAuthority) -> Option<Self> {
        let hub = self.hub.as_ref()?.with_authority(authority).ok()?;
        Some(Self::from_hub(hub))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateThreadMaterialization {
    Created,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForkThreadMaterialization {
    Forked { source_thread_id: SurfaceThreadId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadSettingsReceipt {
    Unchanged {
        host_revision: SettingsRevision,
        thread_revision: Option<SettingsRevision>,
    },
    Committed {
        receipt: SurfaceRuntimeSettingsReceipt,
    },
}

#[derive(Clone)]
pub enum CreateThreadOutput {
    Recorded {
        surface: RuntimeSurfaceHandle,
        thread: SurfaceThreadSnapshot,
        materialization: CreateThreadMaterialization,
        catalog_receipt: SurfaceSessionCatalogReceipt,
        settings_receipt: ThreadSettingsReceipt,
    },
    Ephemeral {
        surface: RuntimeSurfaceHandle,
        thread: SurfaceThreadSnapshot,
        materialization: CreateThreadMaterialization,
        settings_receipt: ThreadSettingsReceipt,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenThreadMaterialization {
    AttachedLive,
    MaterializedLive,
}

#[derive(Clone)]
pub struct OpenThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: OpenThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadThreadRecovery {
    Clean,
    RecoveryRequired,
    FinalizationReconciled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadThreadMaterialization {
    LoadedCold { recovery: LoadThreadRecovery },
}

#[derive(Clone)]
pub struct LoadThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: LoadThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone)]
pub struct ForkThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
    pub materialization: ForkThreadMaterialization,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
    pub settings_receipt: ThreadSettingsReceipt,
}

#[derive(Clone)]
pub struct ResolveRunningThreadOutput {
    pub surface: RuntimeSurfaceHandle,
    pub thread: SurfaceThreadSnapshot,
}

#[derive(Clone)]
pub struct ResumeLatestGoalOutput {
    pub surface: RuntimeSurfaceHandle,
    pub goal: SurfaceGoal,
    pub goal_receipt: SurfaceGoalStoreReceipt,
    pub goal_cursor: SurfaceCursor,
    pub operation_id: SurfaceOperationId,
    pub operation_cursor: SurfaceCursor,
    pub waiter: OperationWaiterHandle,
    pub catalog_receipt: SurfaceSessionCatalogReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryPinResult {
    NotRequested,
    Committed {
        thread_id: SurfaceThreadId,
        cursor: SurfaceCursor,
    },
    Pending {
        thread_id: SurfaceThreadId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryMutationOutput {
    pub memory_receipt: SurfaceMemoryReceipt,
    pub pin: MemoryPinResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustRead {
    pub canonical_path: CanonicalPath,
    pub matched_ancestor: CanonicalPath,
    pub effective_level: FolderTrustLevel,
    pub trust_revision: TrustRevision,
    pub policy_epoch: PolicyEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustMutationOutput {
    pub receipt: SurfaceFolderTrustReceipt,
    pub barrier_plan: PolicyRevocationBarrierPlan,
    pub pending: Vec<PolicyRevocationSubject>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsRead {
    pub host_revision: SettingsRevision,
    pub thread_revision: Option<SettingsRevision>,
    pub effective: SurfaceRuntimeSettings,
    pub pending: Option<SurfaceRuntimeSettings>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettingsMutationOutput {
    pub receipt: SurfaceRuntimeSettingsReceipt,
    pub thread_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataMutationOutput {
    pub metadata: SurfaceSessionMetadata,
    pub receipt: SurfaceSessionMetadataReceipt,
    pub thread_cursor: Option<SurfaceCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClosedThreadReceipt {
    Recorded {
        thread_id: SurfaceThreadId,
        operation_terminals: Vec<OperationTerminalAtCursor>,
        closed_cursor: SurfaceCursor,
        catalog_receipt: SurfaceSessionCatalogReceipt,
    },
    Ephemeral {
        thread_id: SurfaceThreadId,
        persistence: EphemeralThreadPersistence,
        operation_terminals: Vec<OperationTerminalAtCursor>,
        closed_cursor: SurfaceCursor,
    },
}

pub type CloseThreadOutput = ClosedThreadReceipt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownHostOutput {
    pub host_incarnation: HostIncarnation,
    pub host_receipt: SurfaceHostShutdownReceipt,
    pub closed_threads: Vec<ClosedThreadReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetainedShutdownOutput {
    CloseThread { output: CloseThreadOutput },
    ShutdownHost { output: ShutdownHostOutput },
}

#[derive(Clone, Eq, PartialEq)]
pub enum ReconcileHostMutationOutput {
    Settlement {
        result: RuntimeSurfaceMutationResult,
    },
    CloseThread {
        result: MutationReply<CloseThreadOutput>,
    },
    ShutdownHost {
        result: MutationReply<ShutdownHostOutput>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownBarrierState {
    Closing,
    Closed {
        retained_output: RetainedShutdownOutput,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownBarrierRecord {
    pub plan: ShutdownBarrierPlan,
    pub settled: Vec<MutationCommitAck>,
    pub state: ShutdownBarrierState,
}

pub struct StoreProviderCredential {
    pub request_id: SurfaceRequestId,
    pub provider: NonEmptyText,
    pub secret: ZeroizingProcessLocalSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreProviderCredentialError {
    InvalidInput,
    StoreUnavailable,
    PermissionDenied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreProviderCredentialResult {
    Committed {
        credential_revision: BootstrapCredentialRevision,
        provider: NonEmptyText,
    },
    Uncommitted {
        error: StoreProviderCredentialError,
    },
}

#[cfg(test)]
mod closed_command_domain_tests {
    use super::*;
    use crate::runtime_surface::identity::{
        CursorSourceRevision, HostMonotonicClockId, MonotonicInstant, MonotonicTick,
        SurfaceResponseReceiptId,
    };
    use crate::runtime_surface::interaction::{
        ApplicableAuthorityFingerprint, SurfaceInteractionSafeProjection, SurfaceUserInputDecision,
    };
    use crate::runtime_surface::operation::{
        OperationIngressCorrelation, OperationSettingsPreparation, ReplayabilityRequest,
        SurfaceInputRequestBlock, SurfaceReasoningEffort, UsageTotals,
    };
    use crate::runtime_surface::projection::{
        GoalUsage, SurfaceGoalReceiptState, SurfaceGoalState, SurfaceTaskStatus, SurfaceTaskType,
        SurfaceWorkflowStatus,
    };
    use std::collections::{BTreeSet, HashSet};

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    fn request_id(seed: u8) -> SurfaceRequestId {
        SurfaceRequestId::try_from_bytes(uuid_v7_bytes(seed)).unwrap()
    }

    fn operation_id(seed: u8) -> SurfaceOperationId {
        SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed)).unwrap()
    }

    fn thread_id(seed: u8) -> SurfaceThreadId {
        SurfaceThreadId::try_from_bytes([seed; 16]).unwrap()
    }

    fn host_incarnation(seed: u8) -> HostIncarnation {
        HostIncarnation::try_from_bytes(uuid_v7_bytes(seed)).unwrap()
    }

    fn digest(seed: u8) -> Sha256Digest {
        Sha256Digest::new([seed; 32])
    }

    fn canonical_path() -> CanonicalPath {
        super::super::identity::test_canonical_path("orca-surface")
    }

    fn operation_fence(seed: u8) -> SurfaceOperationFence {
        SurfaceOperationFence {
            thread_id: thread_id(seed),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: operation_id(seed),
            generation_id: SurfaceGenerationId::new(0),
        }
    }

    fn cursor(seed: u8) -> SurfaceCursor {
        SurfaceCursor {
            thread_id: thread_id(seed),
            incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(1).unwrap(),
            },
        }
    }

    fn bound_caller(seed: u8) -> SurfaceBoundCaller {
        SurfaceBoundCaller::new(
            SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            Some(SurfaceConnectionId::try_from_bytes(uuid_v7_bytes(seed)).unwrap()),
        )
    }

    fn host_caller(seed: u8) -> SurfaceHostBoundCaller {
        SurfaceHostBoundCaller::new(
            host_incarnation(seed),
            Some(SurfaceConnectionId::try_from_bytes(uuid_v7_bytes(seed)).unwrap()),
        )
    }

    fn input_request() -> SurfaceInputRequest {
        SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new("typed input"),
            }])
            .unwrap(),
        }
    }

    fn operation_request() -> OperationRequestIntent {
        OperationRequestIntent {
            correlation: OperationIngressCorrelation::TuiUser,
            kind: OperationKind::UserTurn,
            input: Some(input_request()),
            replayability: ReplayabilityRequest::CaptureReplayableCapsule,
            settings_preparation: OperationSettingsPreparation::UseCurrent {
                expected_settings_revision: SettingsRevision::try_new(1).unwrap(),
                expected_policy_epoch: PolicyEpoch::try_new(1).unwrap(),
            },
        }
    }

    fn goal_fence() -> SurfaceGoalFence {
        SurfaceGoalFence {
            goal_id: SurfaceGoalId::try_new("goal-1").unwrap(),
            goal_revision: GoalRevision::try_new(1).unwrap(),
            goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
        }
    }

    fn session_read_token(seed: u8) -> SessionReadToken {
        SessionReadToken {
            thread_id: thread_id(seed),
            durable_revision: DurableRevision::try_new(1).unwrap(),
            metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
            snapshot_digest: digest(seed),
        }
    }

    fn policy_plan() -> PolicyRevocationBarrierPlan {
        PolicyRevocationBarrierPlan {
            canonical_path: canonical_path(),
            trust_revision: TrustRevision::try_new(2).unwrap(),
            policy_epoch: PolicyEpoch::try_new(2).unwrap(),
            expected_owner_leases: Vec::new(),
            expected_resources: Vec::new(),
            plan_digest: digest(71),
        }
    }

    fn runtime_settings() -> SurfaceRuntimeSettings {
        SurfaceRuntimeSettings {
            model: NonEmptyText::try_new("deepseek-v4-pro").unwrap(),
            reasoning_effort: SurfaceReasoningEffort::High,
            approval_mode: SurfaceApprovalMode::AutoEdit,
            cwd: canonical_path(),
            workspace_roots: vec![canonical_path()],
            active_permission_profile: None,
            permission_rules: SurfacePermissionRuleSet {
                ordered_rules: Vec::new(),
                digest: digest(72),
            },
            additional_working_directories: Vec::new(),
            network_permissions: SurfaceNetworkPermissions {
                enabled: Some(true),
                domains: Vec::new(),
            },
            policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        }
    }

    fn settings_snapshot() -> SurfaceSettingsSnapshot {
        SurfaceSettingsSnapshot {
            host_revision: SettingsRevision::try_new(1).unwrap(),
            thread_revision: SettingsRevision::try_new(1).unwrap(),
            effective: runtime_settings(),
            pending: None,
            frozen_generation_revision: None,
        }
    }

    #[test]
    fn surface_authority_binds_connection_identity() {
        let host = host_incarnation(41);
        let thread = thread_id(42);
        let connection = SurfaceConnectionId::try_from_bytes(uuid_v7_bytes(43)).unwrap();
        let authority = SurfaceAttachAuthority::new(
            host,
            thread,
            SurfaceAttachmentRole::Jsonl,
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap(),
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap(),
            BTreeSet::new(),
        );
        let bound = authority.with_connection_id(connection.clone());
        assert_eq!(bound.connection_id(), Some(&connection));
    }

    #[test]
    fn surface_host_connection_binding_is_immutable_per_adapter_instance() {
        let grant =
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap();
        let host = RuntimeSurfaceHostHandle::new(host_incarnation(44), grant, None);
        let first = host.bind_new_connection();
        let first_clone = first.clone();
        let second = host.bind_new_connection();

        assert!(host.connection_id().is_none());
        assert_eq!(first.connection_id(), first_clone.connection_id());
        assert_ne!(first.connection_id(), second.connection_id());
    }

    fn thread_snapshot(seed: u8) -> SurfaceThreadSnapshot {
        SurfaceThreadSnapshot {
            thread_id: thread_id(seed),
            owner_epoch: ThreadOwnerEpoch::new(1),
            persistence: ThreadPersistence::RecordedCatalogued,
            title: DisplayText::new("typed thread"),
            metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
            created_at: UnixMillis::new(1),
            updated_at: UnixMillis::new(2),
            cwd: canonical_path(),
            workspace_roots: vec![canonical_path()],
            closed: false,
        }
    }

    fn surface_handle(seed: u8) -> RuntimeSurfaceHandle {
        let capabilities = NonEmptySet::try_new(BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
        ]))
        .unwrap();
        let host = host_incarnation(seed);
        let thread = thread_id(seed);
        let authority = SurfaceAttachAuthority::new(
            host.clone(),
            thread.clone(),
            SurfaceAttachmentRole::Tui,
            capabilities.clone(),
            capabilities,
            BTreeSet::from([SurfaceInteractionKind::UserInput]),
        );
        RuntimeSurfaceHandle::new(host, thread, authority)
    }

    fn goal(seed: u8) -> SurfaceGoal {
        SurfaceGoal {
            goal_id: SurfaceGoalId::try_new(format!("goal-{seed}")).unwrap(),
            thread_id: thread_id(seed),
            goal_revision: GoalRevision::try_new(1).unwrap(),
            goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
            catalog_revision: GoalCatalogRevision::try_new(1).unwrap(),
            receipt_digest: digest(seed),
            objective: NonEmptyText::try_new("finish typed surface").unwrap(),
            objective_revision: GoalObjectiveRevision::new(1),
            state: SurfaceGoalState::Active,
            token_budget: Some(10_000),
            usage: GoalUsage {
                charged_input_tokens: 1,
                output_tokens: 2,
                cache_tokens: 0,
                verifier_tokens: 0,
                cost_micros: 3,
                elapsed_seconds: 4,
            },
            current_run: None,
            last_transition: None,
        }
    }

    fn goal_receipt(seed: u8) -> SurfaceGoalStoreReceipt {
        let goal = goal(seed);
        SurfaceGoalStoreReceipt {
            goal_id: goal.goal_id,
            goal_revision: goal.goal_revision,
            objective_revision: goal.objective_revision,
            catalog_revision: goal.catalog_revision,
            goal_owner_epoch: goal.goal_owner_epoch,
            row_state: SurfaceGoalReceiptState::Present {
                state: goal.state,
                current_run: None,
            },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            receipt_digest: digest(seed),
        }
    }

    fn task(seed: u8) -> SurfaceTask {
        SurfaceTask {
            task_id: SurfaceTaskId::try_new(format!("task-{seed}")).unwrap(),
            revision: TaskRevision::try_new(1).unwrap(),
            task_type: SurfaceTaskType::MainSession,
            status: SurfaceTaskStatus::Running,
            backgrounded: false,
            description: DisplayText::new("typed task"),
            created_at: UnixMillis::new(1),
            started_at: Some(UnixMillis::new(2)),
            completed_at: None,
            parent_operation: Some(operation_id(seed)),
            parent_task_id: None,
            background_fence: None,
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        }
    }

    fn workflow(seed: u8) -> SurfaceWorkflow {
        SurfaceWorkflow {
            workflow_run_id: SurfaceWorkflowRunId::try_new(format!("workflow-{seed}")).unwrap(),
            task_id: SurfaceTaskId::try_new(format!("workflow-task-{seed}")).unwrap(),
            revision: WorkflowRevision::try_new(1).unwrap(),
            name: NonEmptyText::try_new("typed workflow").unwrap(),
            status: SurfaceWorkflowStatus::Running,
            phases: Vec::new(),
            agents: Vec::new(),
            result: None,
            error: None,
            parent: Some(operation_fence(seed)),
        }
    }

    fn commit_class(seed: u8) -> CommitClass {
        CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(1).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        }
    }

    fn operation_terminal(seed: u8) -> OperationTerminalAtCursor {
        OperationTerminalAtCursor {
            operation_id: operation_id(seed),
            terminal: OperationTerminal::Succeeded {
                usage: UsageTotals {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 3,
                },
            },
            completion_proof: super::super::SurfaceOperationCompletionProof::unverified(
                "test terminal has no verifier proof",
            ),
            cursor: cursor(seed),
            commit_class: commit_class(seed),
            batch_digest: digest(seed),
        }
    }

    fn invalid_mutation(seed: u8) -> UncommittedMutation {
        UncommittedMutation::Invalid {
            request_id: request_id(seed),
            target: Some(MutationTarget::Thread {
                thread_id: thread_id(seed),
            }),
            error: InvalidMutationError::new(SurfaceMutationError {
                code: SurfaceMutationErrorCode::InvalidInput,
                message: DisplayText::new("invalid fixture request"),
                winning_request_id: None,
                current_revision: None,
            }),
        }
    }

    fn uncommitted_reply<T>(seed: u8) -> MutationReply<T> {
        MutationReply::Uncommitted {
            mutation: invalid_mutation(seed),
        }
    }

    fn read_not_found<T>(seed: u8) -> SurfaceReadResult<T> {
        SurfaceReadResult::NotFound {
            request_id: request_id(seed),
            error: SurfaceReadError {
                class: SurfaceReadErrorClass::NotFound,
                code: SurfaceReadErrorCode::NotFound,
                message: DisplayText::new("fixture not found"),
                current_revision: None,
            },
        }
    }

    fn session_summary(seed: u8) -> SurfaceSessionSummary {
        SurfaceSessionSummary {
            thread_id: thread_id(seed),
            title: DisplayText::new("typed session"),
            cwd: canonical_path(),
            provider: NonEmptyText::try_new("deepseek").unwrap(),
            model: Some(NonEmptyText::try_new("deepseek-v4-pro").unwrap()),
            created_at: Rfc3339Timestamp::try_new("2026-07-22T00:00:00Z").unwrap(),
            updated_at: Rfc3339Timestamp::try_new("2026-07-22T00:01:00Z").unwrap(),
            parent_thread_id: None,
            forked: false,
            archived: false,
            approval_mode: Some(SurfaceApprovalMode::AutoEdit),
            active_permission_profile: None,
            permission_rule_count: 0,
            runtime_workspace_roots: vec![canonical_path()],
            additional_working_directories: Vec::new(),
            network_permissions: SurfaceNetworkPermissions {
                enabled: Some(true),
                domains: Vec::new(),
            },
            message_count: 1,
            turn_count: 1,
            metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
            running: true,
        }
    }

    fn session_metadata(seed: u8) -> SurfaceSessionMetadata {
        SurfaceSessionMetadata {
            summary: session_summary(seed),
            runtime_workspace_roots: vec![canonical_path()],
            active_permission_profile: None,
            permission_rules: SurfacePermissionRuleSet {
                ordered_rules: Vec::new(),
                digest: digest(seed),
            },
            additional_working_directories: Vec::new(),
            network_permissions: SurfaceNetworkPermissions {
                enabled: Some(true),
                domains: Vec::new(),
            },
        }
    }

    fn session_catalog_receipt(seed: u8) -> SurfaceSessionCatalogReceipt {
        SurfaceSessionCatalogReceipt {
            catalog_revision: SessionCatalogRevision::try_new(1).unwrap(),
            thread_id: Some(thread_id(seed)),
            action: SurfaceSessionCatalogAction::Opened,
        }
    }

    fn thread_settings_receipt() -> ThreadSettingsReceipt {
        ThreadSettingsReceipt::Unchanged {
            host_revision: SettingsRevision::try_new(1).unwrap(),
            thread_revision: Some(SettingsRevision::try_new(1).unwrap()),
        }
    }

    fn admission_output_is_closed(value: &AdmissionOutput) {
        match value {
            AdmissionOutput::Queued {
                operation_id,
                queue_position,
                lease,
                waiter,
            } => {
                let _ = (operation_id, queue_position, lease, waiter);
            }
            AdmissionOutput::Admitted {
                operation_id,
                first_generation,
                admitted_cursor,
                waiter,
            } => {
                let _ = (operation_id, first_generation, admitted_cursor, waiter);
            }
        }
    }

    fn cancel_output_is_closed(value: &CancelOperationOutput) {
        match value {
            CancelOperationOutput::CancelledBeforeAdmission { terminal }
            | CancelOperationOutput::AlreadyTerminal { terminal } => {
                let _ = terminal;
            }
            CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor,
                waiter,
            } => {
                let _ = (operation_id, accepted_cursor, waiter);
            }
            CancelOperationOutput::FinalizationPending {
                operation_id,
                finalize_intent_id,
                finalization_cursor,
                waiter,
            } => {
                let _ = (
                    operation_id,
                    finalize_intent_id,
                    finalization_cursor,
                    waiter,
                );
            }
        }
    }

    fn cancel_current_result_is_closed(value: &CancelSessionCurrentResult) {
        match value {
            CancelSessionCurrentResult::NoCurrentOperation {
                request_id,
                thread_id,
            } => {
                let _ = (request_id, thread_id);
            }
            CancelSessionCurrentResult::Resolved { mutation } => {
                let _ = mutation;
            }
        }
    }

    fn pause_operation_output_is_closed(value: &PauseGoalOperationOutput) {
        match value {
            PauseGoalOperationOutput::None => {}
            PauseGoalOperationOutput::CancelledBeforeAdmission { terminal } => {
                let _ = terminal;
            }
            PauseGoalOperationOutput::Cancelling {
                operation_id,
                accepted_cursor,
                waiter,
            } => {
                let _ = (operation_id, accepted_cursor, waiter);
            }
        }
    }

    fn transfer_output_is_closed(value: &TransferBackgroundOutput) {
        match value {
            TransferBackgroundOutput::QueuedOnStart {
                operation_id,
                intent_cursor,
            } => {
                let _ = (operation_id, intent_cursor);
            }
            TransferBackgroundOutput::HandedOff {
                background_fence,
                handoff_cursor,
                waiter,
            } => {
                let _ = (background_fence, handoff_cursor, waiter);
            }
        }
    }

    fn respond_disposition_is_closed(value: &RespondInteractionDisposition) {
        match value {
            RespondInteractionDisposition::Resolved { receipt } => {
                let _ = receipt;
            }
            RespondInteractionDisposition::AlreadyResolved { winning_receipt } => {
                let _ = winning_receipt;
            }
        }
    }

    fn mcp_values_are_closed(value: &McpCatalogPageValues) {
        match value {
            McpCatalogPageValues::Tools(values) => {
                let _ = values;
            }
            McpCatalogPageValues::Resources(values) => {
                let _ = values;
            }
            McpCatalogPageValues::ResourceTemplates(values) => {
                let _ = values;
            }
            McpCatalogPageValues::Entry(value) => {
                let _ = value;
            }
        }
    }

    fn thread_page_is_closed(value: &SurfaceThreadPage) {
        match value {
            SurfaceThreadPage::Messages {
                read_token,
                data,
                next_cursor,
                backwards_cursor,
            } => {
                let _ = (read_token, data, next_cursor, backwards_cursor);
            }
            SurfaceThreadPage::Turns {
                read_token,
                data,
                next_cursor,
                backwards_cursor,
            } => {
                let _ = (read_token, data, next_cursor, backwards_cursor);
            }
            SurfaceThreadPage::Items {
                read_token,
                data,
                next_cursor,
                backwards_cursor,
            } => {
                let _ = (read_token, data, next_cursor, backwards_cursor);
            }
        }
    }

    fn create_output_is_closed(value: &CreateThreadOutput) {
        match value {
            CreateThreadOutput::Recorded {
                surface,
                thread,
                materialization,
                catalog_receipt,
                settings_receipt,
            } => {
                let _ = (
                    surface,
                    thread,
                    materialization,
                    catalog_receipt,
                    settings_receipt,
                );
            }
            CreateThreadOutput::Ephemeral {
                surface,
                thread,
                materialization,
                settings_receipt,
            } => {
                let _ = (surface, thread, materialization, settings_receipt);
            }
        }
    }

    fn memory_pin_is_closed(value: &MemoryPinResult) {
        match value {
            MemoryPinResult::NotRequested => {}
            MemoryPinResult::Committed { thread_id, cursor } => {
                let _ = (thread_id, cursor);
            }
            MemoryPinResult::Pending { thread_id } => {
                let _ = thread_id;
            }
        }
    }

    fn jsonl_result_is_closed(value: &JsonlTurnControlResult) {
        match value {
            JsonlTurnControlResult::Idle { request_id, echo } => {
                let _ = (request_id, echo);
            }
            JsonlTurnControlResult::Resolved { mutation } => {
                let _ = mutation;
            }
        }
    }

    fn closed_thread_is_closed(value: &ClosedThreadReceipt) {
        match value {
            ClosedThreadReceipt::Recorded {
                thread_id,
                operation_terminals,
                closed_cursor,
                catalog_receipt,
            } => {
                let _ = (
                    thread_id,
                    operation_terminals,
                    closed_cursor,
                    catalog_receipt,
                );
            }
            ClosedThreadReceipt::Ephemeral {
                thread_id,
                persistence,
                operation_terminals,
                closed_cursor,
            } => {
                let _ = (thread_id, persistence, operation_terminals, closed_cursor);
            }
        }
    }

    #[test]
    fn unknown_opaque_interaction_ids_do_not_allocate_client_response_state() {
        let attachment_id = SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(201)).unwrap();
        let thread_id = thread_id(202);
        let host_incarnation = host_incarnation(203);
        let client = RuntimeSurfaceClientHandle::new(
            attachment_id.clone(),
            thread_id,
            host_incarnation.clone(),
            SurfaceAttachmentGrant {
                attachment_id,
                host_incarnation,
                role: SurfaceAttachmentRole::Tui,
                capabilities: NonEmptySet::try_new(BTreeSet::from([
                    SurfaceCapability::ReadSnapshot,
                    SurfaceCapability::RespondGrantedInteraction,
                ]))
                .unwrap(),
                granted_at: cursor(204),
                expires_at: None,
            },
            None,
            Arc::new(()),
        );

        for index in 0..1_024 {
            assert!(matches!(
                client.respond_interaction(
                    request_id((index % 200) as u8),
                    NonEmptyText::try_new(format!("unknown-{index}")).unwrap(),
                    SurfaceInteractionKind::UserInput,
                    SurfaceClientInteractionAnswer::UserInput {
                        decision: SurfaceUserInputDecision::Cancel,
                    },
                ),
                Err(SurfaceClientCommandError::Unauthorized)
            ));
        }
    }

    fn reconcile_host_output_is_closed(value: &ReconcileHostMutationOutput) {
        match value {
            ReconcileHostMutationOutput::Settlement { result } => {
                let _ = result;
            }
            ReconcileHostMutationOutput::CloseThread { result } => {
                let _ = result;
            }
            ReconcileHostMutationOutput::ShutdownHost { result } => {
                let _ = result;
            }
        }
    }

    #[test]
    fn every_thread_and_host_command_has_a_valid_closed_constructor() {
        let thread_commands = [
            SurfaceCommand::ReserveOperation {
                request_id: request_id(1),
                caller: bound_caller(1),
                intent: operation_request(),
            },
            SurfaceCommand::AdmitReserved {
                request_id: request_id(2),
                caller: bound_caller(2),
                operation_id: operation_id(2),
                admission_lease_id: SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(2))
                    .unwrap(),
            },
            SurfaceCommand::CancelOperation {
                request_id: request_id(3),
                caller: bound_caller(3),
                operation_id: operation_id(3),
            },
            SurfaceCommand::CancelSessionCurrent {
                request_id: request_id(4),
                caller: bound_caller(4),
                legacy_rpc_id_digest: digest(4),
            },
            SurfaceCommand::InterruptGeneration {
                request_id: request_id(5),
                caller: bound_caller(5),
                fence: operation_fence(5),
            },
            SurfaceCommand::PauseGoalOperation {
                request_id: request_id(6),
                caller: bound_caller(6),
                goal_fence: goal_fence(),
            },
            SurfaceCommand::ResumeOperation {
                request_id: request_id(7),
                caller: bound_caller(7),
                operation_id: operation_id(7),
                expected_last_generation: SurfaceGenerationId::new(0),
                resume_source: ResumeSourceWitness::DurableReplay {
                    replayability_digest: digest(7),
                },
            },
            SurfaceCommand::SteerOperation {
                request_id: request_id(8),
                caller: bound_caller(8),
                fence: operation_fence(8),
                input: input_request(),
            },
            SurfaceCommand::TransferBackground {
                request_id: request_id(9),
                caller: bound_caller(9),
                target: BackgroundTarget::ReservedOperation {
                    operation_id: operation_id(9),
                    admission_lease_id: SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(9))
                        .unwrap(),
                },
            },
            SurfaceCommand::RespondInteraction {
                request_id: request_id(10),
                caller: bound_caller(10),
                selector: InteractionSelector::OpaqueRequestId {
                    opaque_request_id: NonEmptyText::try_new("opaque-request").unwrap(),
                    expected_kind: SurfaceInteractionKind::UserInput,
                },
                response: BoundInteractionResponse::new(
                    SurfaceResponseId::try_from_bytes(uuid_v7_bytes(10)).unwrap(),
                    SurfaceClientInteractionAnswer::UserInput {
                        decision: SurfaceUserInputDecision::Cancel,
                    },
                    BrokerInteractionAnswerPolicy::NativeStrict,
                    ApplicableAuthorityFingerprint::not_applicable(),
                ),
            },
            SurfaceCommand::ReconcileMutation {
                token: ReconcileMutationToken::new(
                    request_id(11),
                    MutationTarget::Thread {
                        thread_id: thread_id(11),
                    },
                    SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(11)).unwrap(),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(11)).unwrap(),
                ),
            },
            SurfaceCommand::RetryStartCommit {
                token: RetryStartCommitToken::new(
                    request_id(12),
                    ThreadOwnerEpoch::new(1),
                    operation_fence(12),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(12)).unwrap(),
                ),
            },
            SurfaceCommand::RetryProjection {
                token: RetryLocalProjectionToken::new(
                    request_id(13),
                    MutationTarget::Thread {
                        thread_id: thread_id(13),
                    },
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(13)).unwrap(),
                    ThreadOwnerEpoch::new(1),
                    SurfaceFactFamily::Session,
                    SurfaceEventId::try_from_bytes(uuid_v7_bytes(13)).unwrap(),
                )
                .as_token(),
            },
            SurfaceCommand::RetryFinalization {
                token: RetryFinalizationToken::new(
                    request_id(14),
                    thread_id(14),
                    operation_id(14),
                    SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(14)).unwrap(),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(14)).unwrap(),
                    ThreadOwnerEpoch::new(1),
                    digest(14),
                ),
            },
            SurfaceCommand::ManualCompact {
                request_id: request_id(15),
                caller: bound_caller(15),
                expected_context_revision: ContextRevision::try_new(1).unwrap(),
            },
            SurfaceCommand::Backtrack {
                request_id: request_id(16),
                caller: bound_caller(16),
                expected_cursor: cursor(16),
                target: LastUserTurn::MostRecent,
            },
            SurfaceCommand::TaskControl {
                request_id: request_id(17),
                caller: bound_caller(17),
                action: TaskControlAction::Stop {
                    fence: SurfaceTaskFence {
                        task_id: SurfaceTaskId::try_new("task-17").unwrap(),
                        task_revision: TaskRevision::try_new(1).unwrap(),
                        background_owner: None,
                    },
                },
            },
            SurfaceCommand::WorkflowControl {
                request_id: request_id(18),
                caller: bound_caller(18),
                action: WorkflowControlAction::Launch {
                    catalog_entry_id: SurfaceCatalogEntryId::try_new("workflow-18").unwrap(),
                    observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                    args: vec![(
                        NonEmptyText::try_new("key").unwrap(),
                        DisplayText::new("value"),
                    )],
                    parent: Some(operation_fence(18)),
                },
            },
            SurfaceCommand::GoalMutation {
                request_id: request_id(19),
                caller: bound_caller(19),
                action: GoalMutationAction::SetAndRun {
                    expected_goal: ExpectedGoal::None,
                    objective: NonEmptyText::try_new("prove the contract").unwrap(),
                    token_budget: Some(10_000),
                    input: GoalRunInput::Supplied {
                        request: input_request(),
                    },
                },
            },
            SurfaceCommand::SettingsMutation {
                request_id: request_id(20),
                caller: host_caller(20),
                host_incarnation: host_incarnation(20),
                expected_thread_revision: SettingsRevision::try_new(1).unwrap(),
                patch: RuntimeSettingsPatch::SetModel {
                    model: NonEmptyText::try_new("deepseek-v4-pro").unwrap(),
                },
            },
            SurfaceCommand::McpCatalogQuery {
                request_id: request_id(21),
                caller: bound_caller(21),
                expected_revision: Some(McpCatalogRevision::try_new(1).unwrap()),
                query: McpCatalogQuery::Lookup {
                    id: SurfaceCatalogEntryId::try_new("mcp-entry").unwrap(),
                },
            },
            SurfaceCommand::PinnedContextMutation {
                request_id: request_id(22),
                caller: bound_caller(22),
                action: PinnedContextAction::Clear {
                    expected_revision: PinnedContextRevision::try_new(1).unwrap(),
                },
            },
        ];
        assert_eq!(thread_commands.len(), 22);

        let host_commands = [
            SurfaceHostCommand::ListSessions {
                request_id: request_id(31),
                page: SessionPageRequest {
                    filters: SessionListFilter {
                        cwd: vec![canonical_path()],
                        providers: SessionSetFilter::Any,
                        models: SessionSetFilter::Any,
                        relation: None,
                        archived: SessionListArchiveFilter::ActiveOnly,
                    },
                    search_term: None,
                    sort_key: SessionSortKey::UpdatedAt,
                    direction: SortDirection::Descending,
                    cursor: None,
                    limit: SurfacePageLimit::try_session_catalog(10).unwrap(),
                },
            },
            SurfaceHostCommand::SearchSessions {
                request_id: request_id(32),
                search: SessionSearchRequest {
                    query: NonEmptyText::try_new("surface").unwrap(),
                    archived: SessionSearchArchiveFilter::ActiveAndArchived,
                    sort_key: SessionSortKey::RecencyAt,
                    direction: SortDirection::Descending,
                    cursor: None,
                    limit: SurfacePageLimit::try_session_catalog(10).unwrap(),
                },
            },
            SurfaceHostCommand::ReadSessionMetadata {
                request_id: request_id(33),
                thread_id: thread_id(33),
            },
            SurfaceHostCommand::ReadSession {
                request_id: request_id(34),
                thread_id: thread_id(34),
                include_messages: true,
                include_turns: true,
            },
            SurfaceHostCommand::ReadThreadPage {
                request_id: request_id(35),
                thread_id: thread_id(35),
                query: ThreadPageQuery::Messages {
                    direction: SortDirection::Ascending,
                },
                read_token: Some(session_read_token(35)),
                cursor: None,
                limit: SurfacePageLimit::try_thread_page(20).unwrap(),
            },
            SurfaceHostCommand::CreateThread {
                request_id: request_id(36),
                spec: SurfaceThreadCreateSpec {
                    title: DisplayText::new("typed thread"),
                    persistence: ThreadPersistence::RecordedCatalogued,
                    settings_overrides: Vec::new(),
                    mcp_servers: Vec::new(),
                    parent_thread_id: None,
                },
            },
            SurfaceHostCommand::OpenThread {
                request_id: request_id(37),
                thread_id: thread_id(37),
                mode: OpenThreadMode::LiveOrMaterialize,
                expected_settings_digest: Some(digest(37)),
            },
            SurfaceHostCommand::LoadThread {
                request_id: request_id(38),
                thread_id: thread_id(38),
                expected_settings_digest: Some(digest(38)),
                settings_overrides: Vec::new(),
                mcp_servers: Vec::new(),
            },
            SurfaceHostCommand::ForkThread {
                request_id: request_id(39),
                source_thread_id: thread_id(39),
                source_read_token: session_read_token(39),
                title: Some(DisplayText::new("fork")),
                settings_overrides: Vec::new(),
            },
            SurfaceHostCommand::ResolveRunningThread {
                request_id: request_id(40),
                thread_id: thread_id(40),
                mode: LiveOnly::LiveOnly,
            },
            SurfaceHostCommand::ResumeLatestActiveGoal {
                request_id: request_id(41),
                expected_goal_store_revision: Some(GoalCatalogRevision::try_new(1).unwrap()),
            },
            SurfaceHostCommand::UpdateSessionMetadata {
                request_id: request_id(42),
                thread_id: thread_id(42),
                precondition: SessionMetadataPrecondition::Exact {
                    revision: SessionMetadataRevision::try_new(1).unwrap(),
                },
                patch: SessionMetadataPatch::SetTitle {
                    title: DisplayText::new("renamed"),
                },
            },
            SurfaceHostCommand::QueryInputCatalog {
                request_id: request_id(43),
                context: InputCatalogContext::HostDefaults {
                    host_incarnation: host_incarnation(43),
                    settings_revision: SettingsRevision::try_new(1).unwrap(),
                },
                expected_revision: Some(InputCatalogRevision::try_new(1).unwrap()),
                query: InputCatalogQuery::Lookup {
                    id: SurfaceCatalogEntryId::try_new("input-entry").unwrap(),
                },
            },
            SurfaceHostCommand::ControlJsonlTurn {
                request_id: request_id(44),
                expected_thread_id: Some(thread_id(44)),
                legacy_turn_id: LegacyTurnId(DisplayText::new("legacy-turn")),
                action: JsonlTurnControlAction::Interrupt,
            },
            SurfaceHostCommand::RememberMemory {
                request_id: request_id(45),
                scope: MemoryScope::User {
                    expected_memory_revision: Some(MemoryRevision::try_new(1).unwrap()),
                },
                note: NonEmptyText::try_new("remember typed surface").unwrap(),
                pin_to_thread: Some(thread_id(45)),
            },
            SurfaceHostCommand::ReconcileMemoryMutation {
                token: ReconcileMemoryMutationToken::new(
                    request_id(46),
                    MutationMemoryScope::User,
                    MemoryRevision::try_new(2).unwrap(),
                    SurfaceCatalogEntryId::try_new("memory-record").unwrap(),
                    thread_id(46),
                    ThreadOwnerEpoch::new(1),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(46)).unwrap(),
                ),
            },
            SurfaceHostCommand::ReadFolderTrust {
                request_id: request_id(47),
                path: canonical_path(),
            },
            SurfaceHostCommand::SetFolderTrust {
                request_id: request_id(48),
                path: canonical_path(),
                expected_trust_revision: TrustRevision::try_new(1).unwrap(),
                level: FolderTrustLevel::Trusted,
            },
            SurfaceHostCommand::ReconcileFolderTrustRevocation {
                token: ReconcileFolderTrustRevocationToken::new(
                    request_id(49),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(49)).unwrap(),
                    policy_plan(),
                ),
            },
            SurfaceHostCommand::ReadRuntimeSettings {
                request_id: request_id(50),
                thread_id: Some(thread_id(50)),
            },
            SurfaceHostCommand::UpdateRuntimeSettings {
                request_id: request_id(51),
                target: RuntimeSettingsTarget::Thread {
                    thread_id: thread_id(51),
                },
                expected: RuntimeSettingsExpectedRevision {
                    host: SettingsRevision::try_new(1).unwrap(),
                    thread: Some(SettingsRevision::try_new(1).unwrap()),
                },
                patch: NonEmptyVec::try_new(vec![RuntimeSettingsPatch::SetReasoning {
                    effort: SurfaceReasoningEffort::High,
                }])
                .unwrap(),
            },
            SurfaceHostCommand::ReconcileHostMutation {
                token: ReconcileHostSettlementToken::new(
                    request_id(52),
                    MutationTarget::RuntimeSettings {
                        host_incarnation: host_incarnation(52),
                        thread_id: Some(thread_id(52)),
                    },
                    SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(52)).unwrap(),
                    host_incarnation(52),
                    SurfaceCommitId::try_from_bytes(uuid_v7_bytes(52)).unwrap(),
                )
                .as_token(),
            },
            SurfaceHostCommand::CloseThread {
                request_id: request_id(53),
                thread_id: thread_id(53),
                expected_owner_epoch: Some(ThreadOwnerEpoch::new(1)),
            },
            SurfaceHostCommand::ShutdownHost {
                request_id: request_id(54),
                host_incarnation: host_incarnation(54),
            },
        ];
        assert_eq!(host_commands.len(), 24);

        let mut unique_kinds = HashSet::new();
        for command in &thread_commands {
            unique_kinds.insert(std::mem::discriminant(command));
        }
        assert_eq!(unique_kinds.len(), 22);
        let mut unique_host_kinds = HashSet::new();
        for command in &host_commands {
            unique_host_kinds.insert(std::mem::discriminant(command));
        }
        assert_eq!(unique_host_kinds.len(), 24);
    }

    #[test]
    fn every_command_result_type_has_a_valid_closed_constructor() {
        let lease = ReservationLease::new(
            SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(81)).unwrap(),
            operation_id(81),
            SequenceNumber::new(1),
            host_incarnation(81),
            MonotonicInstant {
                clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(81)).unwrap(),
                tick: MonotonicTick::new(1),
            },
        );
        let reserved_output = ReservedOperationOutput {
            operation_id: operation_id(81),
            lease: lease.clone(),
            requested_cursor: cursor(81),
            waiter: OperationWaiterHandle::new(),
        };
        let admission_output = AdmissionOutput::Queued {
            operation_id: operation_id(82),
            queue_position: 1,
            lease,
            waiter: OperationWaiterHandle::new(),
        };
        let cancel_output = CancelOperationOutput::Accepted {
            operation_id: operation_id(83),
            accepted_cursor: cursor(83),
            waiter: OperationWaiterHandle::new(),
        };
        let cancel_current = CancelSessionCurrentResult::NoCurrentOperation {
            request_id: request_id(84),
            thread_id: thread_id(84),
        };
        let interrupt_output = InterruptOutput {
            fence: operation_fence(85),
            accepted_cursor: cursor(85),
            settlement: InterruptSettlement::SuspendUntilExplicitControl,
            waiter: OperationWaiterHandle::new(),
        };
        let pause_output = PauseGoalOutput {
            goal: goal(86),
            goal_receipt: goal_receipt(86),
            goal_cursor: cursor(86),
            operation: PauseGoalOperationOutput::None,
        };
        let transition_receipt = |seed| ResumeTransitionReceipt {
            role: ResumeTransitionRole::ResumeStarting,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            cursor: cursor(seed),
            commit_class: commit_class(seed),
        };
        let resume_output = ResumeOperationOutput {
            operation_id: operation_id(87),
            generation: operation_fence(87),
            resume_starting: transition_receipt(87),
            generation_reserved: ResumeTransitionReceipt {
                role: ResumeTransitionRole::GenerationReserved,
                ..transition_receipt(88)
            },
            generation_started: ResumeTransitionReceipt {
                role: ResumeTransitionRole::GenerationStarted,
                ..transition_receipt(89)
            },
            waiter: OperationWaiterHandle::new(),
        };
        let steer_output = SteerOutput {
            fence: operation_fence(90),
            input_item_id: SurfaceItemId::new(),
            committed_cursor: cursor(90),
        };
        let transfer_output = TransferBackgroundOutput::QueuedOnStart {
            operation_id: operation_id(91),
            intent_cursor: cursor(91),
        };
        let interaction_receipt = SurfaceInteractionResolutionReceipt {
            response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(92)).unwrap(),
            receipt_id: SurfaceResponseReceiptId::try_from_bytes(uuid_v7_bytes(92)).unwrap(),
            kind: SurfaceInteractionKind::UserInput,
            safe_projection: SurfaceInteractionSafeProjection::UserInput { answered: false },
        };
        let respond_output = RespondInteractionOutput {
            interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(92)).unwrap(),
            attempted_response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(93)).unwrap(),
            disposition: RespondInteractionDisposition::Resolved {
                receipt: interaction_receipt,
            },
            projected_cursor: Some(cursor(92)),
        };
        let terminal_output = operation_terminal(94);
        let maintenance_output = MaintenanceOperationOutput {
            operation_id: operation_id(95),
            admitted_cursor: cursor(95),
            waiter: OperationWaiterHandle::new(),
        };
        let task_output = TaskControlOutput {
            task: task(96),
            cursor: cursor(96),
        };
        let workflow_output = WorkflowControlOutput {
            workflow: workflow(97),
            operation_id: Some(operation_id(97)),
            cursor: cursor(97),
            waiter: Some(OperationWaiterHandle::new()),
        };
        let goal_output = GoalMutationOutput {
            goal: Some(goal(98)),
            goal_receipt: goal_receipt(98),
            change_cursor: cursor(98),
            operation_id: Some(operation_id(98)),
            waiter: Some(OperationWaiterHandle::new()),
        };
        let settings_output = SettingsMutationOutput {
            settings: settings_snapshot(),
            cursor: cursor(99),
        };
        let mcp_output = McpCatalogPage {
            revision: McpCatalogRevision::try_new(1).unwrap(),
            values: McpCatalogPageValues::Tools(Vec::new()),
            next_cursor: None,
        };
        let pinned_output = PinnedContextMutationOutput {
            snapshot: SurfacePinnedContextSnapshot {
                revision: PinnedContextRevision::try_new(1).unwrap(),
                entries: Vec::new(),
            },
            cursor: cursor(100),
        };
        let runtime_mutation = RuntimeSurfaceMutationResult::Uncommitted(invalid_mutation(101));

        let _: MutationReply<ReservedOperationOutput> = uncommitted_reply(81);
        let _: MutationReply<AdmissionOutput> = uncommitted_reply(82);
        let _: MutationReply<CancelOperationOutput> = uncommitted_reply(83);
        let _: MutationReply<InterruptOutput> = uncommitted_reply(85);
        let _: MutationReply<PauseGoalOutput> = uncommitted_reply(86);
        let _: MutationReply<ResumeOperationOutput> = uncommitted_reply(87);
        let _: MutationReply<SteerOutput> = uncommitted_reply(90);
        let _: MutationReply<TransferBackgroundOutput> = uncommitted_reply(91);
        let _: MutationReply<RespondInteractionOutput> = uncommitted_reply(92);
        let _: MutationReply<OperationTerminalAtCursor> = uncommitted_reply(94);
        let _: MutationReply<MaintenanceOperationOutput> = uncommitted_reply(95);
        let _: MutationReply<TaskControlOutput> = uncommitted_reply(96);
        let _: MutationReply<WorkflowControlOutput> = uncommitted_reply(97);
        let _: MutationReply<GoalMutationOutput> = uncommitted_reply(98);
        let _: MutationReply<SettingsMutationOutput> = uncommitted_reply(99);
        let _: SurfaceReadResult<McpCatalogPage> = read_not_found(99);
        let _: MutationReply<PinnedContextMutationOutput> = uncommitted_reply(100);

        let session_page = SurfaceSessionSummaryPage {
            catalog_revision: SessionCatalogRevision::try_new(1).unwrap(),
            data: vec![session_summary(111)],
            next_cursor: None,
            backwards_cursor: None,
        };
        let search_page = SurfaceSessionSearchPage {
            catalog_revision: SessionCatalogRevision::try_new(1).unwrap(),
            data: vec![SurfaceSessionSearchHit {
                thread: session_summary(112),
                snippet: DisplayText::new("typed surface"),
            }],
            next_cursor: None,
            backwards_cursor: None,
        };
        let metadata_output = ReadSessionMetadataOutput {
            metadata: session_metadata(113),
            read_token: session_read_token(113),
        };
        let session_output = SurfaceSessionReadBundle {
            metadata: session_metadata(114),
            read_token: session_read_token(114),
            messages: Vec::new(),
            turns: Vec::new(),
        };
        let thread_page = SurfaceThreadPage::Messages {
            read_token: session_read_token(115),
            data: Vec::new(),
            next_cursor: None,
            backwards_cursor: None,
        };
        let create_output = CreateThreadOutput::Ephemeral {
            surface: surface_handle(116),
            thread: thread_snapshot(116),
            materialization: CreateThreadMaterialization::Created,
            settings_receipt: thread_settings_receipt(),
        };
        let open_output = OpenThreadOutput {
            surface: surface_handle(117),
            thread: thread_snapshot(117),
            materialization: OpenThreadMaterialization::AttachedLive,
            catalog_receipt: session_catalog_receipt(117),
            settings_receipt: thread_settings_receipt(),
        };
        let load_output = LoadThreadOutput {
            surface: surface_handle(118),
            thread: thread_snapshot(118),
            materialization: LoadThreadMaterialization::LoadedCold {
                recovery: LoadThreadRecovery::Clean,
            },
            catalog_receipt: session_catalog_receipt(118),
            settings_receipt: thread_settings_receipt(),
        };
        let fork_output = ForkThreadOutput {
            surface: surface_handle(119),
            thread: thread_snapshot(119),
            materialization: ForkThreadMaterialization::Forked {
                source_thread_id: thread_id(118),
            },
            catalog_receipt: session_catalog_receipt(119),
            settings_receipt: thread_settings_receipt(),
        };
        let resolve_output = ResolveRunningThreadOutput {
            surface: surface_handle(120),
            thread: thread_snapshot(120),
        };
        let resume_goal_output = ResumeLatestGoalOutput {
            surface: surface_handle(121),
            goal: goal(121),
            goal_receipt: goal_receipt(121),
            goal_cursor: cursor(121),
            operation_id: operation_id(121),
            operation_cursor: cursor(121),
            waiter: OperationWaiterHandle::new(),
            catalog_receipt: session_catalog_receipt(121),
        };
        let session_metadata_output = SessionMetadataMutationOutput {
            metadata: session_metadata(122),
            receipt: SurfaceSessionMetadataReceipt {
                thread_id: thread_id(122),
                metadata_revision: SessionMetadataRevision::try_new(2).unwrap(),
                title: DisplayText::new("updated"),
            },
            thread_cursor: Some(cursor(122)),
        };
        let input_catalog_output = SurfaceInputCatalogPage {
            revision: InputCatalogRevision::try_new(1).unwrap(),
            data: Vec::new(),
            next_cursor: None,
        };
        let jsonl_controlled_output = JsonlTurnControlledOutput {
            operation_id: operation_id(123),
            echo: JsonlResolvedTurnControlWireEcho {
                legacy_turn_id: LegacyTurnId(DisplayText::new("legacy-turn")),
                action: JsonlTurnControlWireAction::Interrupt,
                status: JsonlResolvedTurnControlStatus::Interrupted,
                legacy_input: None,
            },
            committed_cursor: cursor(123),
            input_item_id: None,
        };
        let jsonl_result = JsonlTurnControlResult::Idle {
            request_id: request_id(123),
            echo: JsonlIdleTurnControlWireEcho {
                legacy_turn_id: LegacyTurnId(DisplayText::new("missing-turn")),
                action: JsonlTurnControlWireAction::Interrupt,
                status: JsonlIdleTurnControlStatus::Idle,
                legacy_input: None,
            },
        };
        let memory_output = MemoryMutationOutput {
            memory_receipt: SurfaceMemoryReceipt {
                scope: MutationMemoryScope::User,
                record_id: SurfaceCatalogEntryId::try_new("memory-record").unwrap(),
                memory_revision: MemoryRevision::try_new(2).unwrap(),
                display_path: canonical_path(),
            },
            pin: MemoryPinResult::NotRequested,
        };
        let folder_read = FolderTrustRead {
            canonical_path: canonical_path(),
            matched_ancestor: canonical_path(),
            effective_level: FolderTrustLevel::Trusted,
            trust_revision: TrustRevision::try_new(1).unwrap(),
            policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        };
        let folder_output = FolderTrustMutationOutput {
            receipt: SurfaceFolderTrustReceipt {
                canonical_path: canonical_path(),
                old_effective_level: FolderTrustLevel::Untrusted,
                new_effective_level: FolderTrustLevel::Trusted,
                trust_revision: TrustRevision::try_new(2).unwrap(),
                policy_epoch: PolicyEpoch::try_new(2).unwrap(),
                reload_required: true,
                reconciliation_proof: None,
            },
            barrier_plan: policy_plan(),
            pending: Vec::new(),
        };
        let settings_receipt = SurfaceRuntimeSettingsReceipt {
            host_revision: SettingsRevision::try_new(2).unwrap(),
            thread_revision: Some(SettingsRevision::try_new(2).unwrap()),
            effective: runtime_settings(),
            pending: None,
        };
        let settings_read = RuntimeSettingsRead {
            host_revision: SettingsRevision::try_new(2).unwrap(),
            thread_revision: Some(SettingsRevision::try_new(2).unwrap()),
            effective: runtime_settings(),
            pending: None,
        };
        let runtime_settings_output = RuntimeSettingsMutationOutput {
            receipt: settings_receipt,
            thread_cursor: Some(cursor(124)),
        };
        let reconcile_host_output = ReconcileHostMutationOutput::Settlement {
            result: RuntimeSurfaceMutationResult::Uncommitted(invalid_mutation(125)),
        };
        let close_output = ClosedThreadReceipt::Ephemeral {
            thread_id: thread_id(126),
            persistence: EphemeralThreadPersistence::EphemeralAttached,
            operation_terminals: vec![operation_terminal(126)],
            closed_cursor: cursor(126),
        };
        let shutdown_output = ShutdownHostOutput {
            host_incarnation: host_incarnation(127),
            host_receipt: SurfaceHostShutdownReceipt {
                host_incarnation: host_incarnation(127),
                lifecycle_revision: HostLifecycleRevision::try_new(2).unwrap(),
                barrier_id: SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(127)).unwrap(),
                shutdown_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(127)).unwrap(),
                stage: SurfaceHostShutdownStage::Last,
                closed_at: UnixMillis::new(10),
            },
            closed_threads: Vec::new(),
        };

        let _: SurfaceReadResult<SurfaceSessionSummaryPage> = read_not_found(111);
        let _: SurfaceReadResult<SurfaceSessionSearchPage> = read_not_found(112);
        let _: SurfaceReadResult<ReadSessionMetadataOutput> = read_not_found(113);
        let _: SurfaceReadResult<SurfaceSessionReadBundle> = read_not_found(114);
        let _: SurfaceReadResult<SurfaceThreadPage> = read_not_found(115);
        let _: MutationReply<CreateThreadOutput> = uncommitted_reply(116);
        let _: MutationReply<OpenThreadOutput> = uncommitted_reply(117);
        let _: MutationReply<LoadThreadOutput> = uncommitted_reply(118);
        let _: MutationReply<ForkThreadOutput> = uncommitted_reply(119);
        let _: SurfaceReadResult<ResolveRunningThreadOutput> = read_not_found(120);
        let _: MutationReply<ResumeLatestGoalOutput> = uncommitted_reply(121);
        let _: MutationReply<SessionMetadataMutationOutput> = uncommitted_reply(122);
        let _: SurfaceReadResult<SurfaceInputCatalogPage> = read_not_found(123);
        let _: MutationReply<JsonlTurnControlledOutput> = uncommitted_reply(123);
        let _: MutationReply<MemoryMutationOutput> = uncommitted_reply(124);
        let _: SurfaceReadResult<FolderTrustRead> = read_not_found(124);
        let _: MutationReply<FolderTrustMutationOutput> = uncommitted_reply(124);
        let _: SurfaceReadResult<RuntimeSettingsRead> = read_not_found(124);
        let _: MutationReply<RuntimeSettingsMutationOutput> = uncommitted_reply(124);
        let _: MutationReply<CloseThreadOutput> = uncommitted_reply(126);
        let _: MutationReply<ShutdownHostOutput> = uncommitted_reply(127);

        admission_output_is_closed(&admission_output);
        cancel_output_is_closed(&cancel_output);
        cancel_current_result_is_closed(&cancel_current);
        pause_operation_output_is_closed(&pause_output.operation);
        transfer_output_is_closed(&transfer_output);
        respond_disposition_is_closed(&respond_output.disposition);
        mcp_values_are_closed(&mcp_output.values);
        thread_page_is_closed(&thread_page);
        create_output_is_closed(&create_output);
        memory_pin_is_closed(&memory_output.pin);
        jsonl_result_is_closed(&jsonl_result);
        closed_thread_is_closed(&close_output);
        reconcile_host_output_is_closed(&reconcile_host_output);

        let _all_values = (
            reserved_output,
            admission_output,
            cancel_output,
            cancel_current,
            interrupt_output,
            pause_output,
            resume_output,
            steer_output,
            transfer_output,
            respond_output,
            terminal_output,
            maintenance_output,
            task_output,
            workflow_output,
            goal_output,
            settings_output,
            mcp_output,
            pinned_output,
            runtime_mutation,
            session_page,
            search_page,
            metadata_output,
            session_output,
            thread_page,
            create_output,
            open_output,
            load_output,
            fork_output,
            resolve_output,
            resume_goal_output,
            session_metadata_output,
            input_catalog_output,
            jsonl_controlled_output,
            jsonl_result,
            memory_output,
            folder_read,
            folder_output,
            settings_read,
            runtime_settings_output,
            reconcile_host_output,
            close_output,
            shutdown_output,
        );
    }

    #[test]
    fn task_transcript_query_is_exposed_as_a_typed_surface_read() {
        let _query: fn(
            &RuntimeSurfaceClientHandle,
            SurfaceRequestId,
            SurfaceTaskId,
            TaskRevision,
        ) -> Result<
            SurfaceReadResult<TaskTranscriptSnapshot>,
            SurfaceClientCommandError,
        > = RuntimeSurfaceClientHandle::read_task_transcript;
    }
}
