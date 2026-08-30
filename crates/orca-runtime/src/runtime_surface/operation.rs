use super::identity::{
    AcpRequestId, ByteOffset, CanonicalDomainName, CanonicalMime, CanonicalPath, CanonicalUri,
    CommitClass, DisplayText, DurableRevision, DurationMillis, GoalObjectiveRevision,
    HostIncarnation, InputCatalogRevision, MonotonicInstant, NonEmptyText, NonEmptyVec,
    PolicyEpoch, SafeDiagnosticText, SequenceNumber, Set, SettingsRevision, Sha256Digest,
    SurfaceAdmissionLeaseId, SurfaceCapability, SurfaceCatalogEntryId, SurfaceCommitId,
    SurfaceConnectionId, SurfaceCursor, SurfaceEventId, SurfaceFinalizeIntentId,
    SurfaceGenerationId, SurfaceGoalId, SurfaceGoalOuterTurnId, SurfaceGoalRunId,
    SurfaceIncarnation, SurfaceInputCorrelationId, SurfaceItemId, SurfaceOperationFence,
    SurfaceOperationId, SurfaceRequestId, SurfaceSettlementId, SurfaceTaskId, SurfaceTurnId,
    SurfaceWorkflowResultId, SurfaceWorkflowRunId, ThreadOwnerEpoch, UnixMillis,
};
use super::projection::SurfaceOperationCompletionProof;
use serde::{Deserialize, Deserializer, Serialize};

pub const SURFACE_RESERVATION_LEASE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInputBindingKind {
    File,
    Directory,
    Skill,
    Plugin,
    Workflow,
    McpResource,
    McpResourceTemplate,
    McpTool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInputBinding {
    pub kind: SurfaceInputBindingKind,
    pub identity: SurfaceCatalogEntryId,
    pub observed_catalog_revision: InputCatalogRevision,
    pub observed_settings_revision: SettingsRevision,
    pub label: NonEmptyText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceLegacyPath(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceLegacyUri(pub DisplayText);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceLegacyMentionKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceLegacyMentionTarget {
    File {
        root: SurfaceLegacyPath,
        path: SurfaceLegacyPath,
        kind: SurfaceLegacyMentionKind,
    },
    Skill {
        id: DisplayText,
        path: SurfaceLegacyPath,
    },
    Plugin {
        name: DisplayText,
        manifest_path: SurfaceLegacyPath,
    },
    Resource {
        server: DisplayText,
        uri: SurfaceLegacyUri,
    },
    ResourceTemplate {
        server: DisplayText,
        uri_template: SurfaceLegacyUri,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceImageDetail {
    Low,
    #[default]
    High,
    Original,
    Auto,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SurfaceImageSource {
    Base64 {
        media_type: CanonicalMime,
        data: String,
        digest: Sha256Digest,
    },
    Url {
        url: CanonicalUri,
    },
    File {
        file_id: NonEmptyText,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInputBindingRequest {
    ExactCatalog {
        kind: SurfaceInputBindingKind,
        identity: SurfaceCatalogEntryId,
        observed_catalog_revision: InputCatalogRevision,
        observed_settings_revision: SettingsRevision,
        label: NonEmptyText,
    },
    LegacyJsonlMention {
        name: DisplayText,
        visible: DisplayText,
        start: ByteOffset,
        end: ByteOffset,
        target: SurfaceLegacyMentionTarget,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInputRequestBlock {
    Text {
        text: DisplayText,
    },
    Binding {
        binding: SurfaceInputBindingRequest,
    },
    ResourceLink {
        uri: CanonicalUri,
        name: NonEmptyText,
        description: Option<DisplayText>,
        mime: Option<CanonicalMime>,
    },
    EmbeddedText {
        uri: CanonicalUri,
        mime: CanonicalMime,
        text: DisplayText,
        digest: Sha256Digest,
    },
    Image {
        source: SurfaceImageSource,
        #[serde(default)]
        detail: SurfaceImageDetail,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInputBlock {
    Text {
        text: DisplayText,
    },
    Binding {
        binding: SurfaceInputBinding,
    },
    ResourceLink {
        uri: CanonicalUri,
        name: NonEmptyText,
        description: Option<DisplayText>,
        mime: Option<CanonicalMime>,
    },
    EmbeddedText {
        uri: CanonicalUri,
        mime: CanonicalMime,
        text: DisplayText,
        digest: Sha256Digest,
    },
    Image {
        source: SurfaceImageSource,
        detail: SurfaceImageDetail,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInput {
    pub blocks: NonEmptyVec<SurfaceInputBlock>,
    pub canonical_text: DisplayText,
    pub bindings_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInputRequest {
    pub blocks: NonEmptyVec<SurfaceInputRequestBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LegacyTurnId(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationOrigin {
    TuiUser,
    Headless,
    TuiWorkflowNotification {
        result_id: SurfaceWorkflowResultId,
    },
    AcpPrompt {
        connection_id: SurfaceConnectionId,
        session_id: NonEmptyText,
        inbound_seq: SequenceNumber,
        rpc_request_id: AcpRequestId,
    },
    JsonlThreadTurn {
        connection_id: SurfaceConnectionId,
        rpc_id_digest: Sha256Digest,
        legacy_turn_id: LegacyTurnId,
    },
    JsonlStatelessSubmit {
        connection_id: SurfaceConnectionId,
        rpc_id_digest: Sha256Digest,
    },
    RuntimeWorkflowResult {
        result_id: SurfaceWorkflowResultId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationIngressCorrelation {
    TuiUser,
    Headless,
    TuiWorkflowNotification {
        result_id: SurfaceWorkflowResultId,
    },
    AcpPrompt {
        session_id: NonEmptyText,
        inbound_seq: SequenceNumber,
        rpc_request_id: AcpRequestId,
    },
    JsonlThreadTurn {
        rpc_id_digest: Sha256Digest,
        legacy_turn_id: LegacyTurnId,
    },
    JsonlStatelessSubmit {
        rpc_id_digest: Sha256Digest,
    },
}

pub struct SurfaceInternalOriginPermit(());

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LastUserTurn {
    MostRecent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ManualCompactionReason {
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationKind {
    UserTurn,
    GoalRun {
        goal_id: SurfaceGoalId,
        goal_run_id: SurfaceGoalRunId,
        initial_objective_revision: GoalObjectiveRevision,
    },
    ManualCompaction {
        reason: ManualCompactionReason,
    },
    Backtrack {
        target: LastUserTurn,
    },
    StandaloneWorkflow {
        workflow: SurfaceCatalogEntryId,
    },
    WorkflowResultFollowup {
        result_id: SurfaceWorkflowResultId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BusyDisposition {
    Queue,
    NotAdmittedImmediately,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InterruptSettlement {
    SuspendUntilExplicitControl,
    TerminalizeCancelledAtInterruptedStopUnlessResumeQueued,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LegacyVisibility {
    PublishAfterAdmitted,
    JsonlBindingsResolvedBeforeTurnStarted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NonReplayableReason {
    HistoryDisabled,
    Redacted,
    SecretInput,
    Missing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReplayabilityRequest {
    CaptureReplayableCapsule,
    NonReplayable { reason: NonReplayableReason },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationRequestIntent {
    pub correlation: OperationIngressCorrelation,
    pub kind: OperationKind,
    pub input: Option<SurfaceInputRequest>,
    pub replayability: ReplayabilityRequest,
    pub settings_preparation: OperationSettingsPreparation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationIntent {
    pub origin: OperationOrigin,
    pub kind: OperationKind,
    pub initial_replayability: Replayability,
    pub busy_disposition: BusyDisposition,
    pub interrupt_settlement: InterruptSettlement,
    pub legacy_visibility: LegacyVisibility,
    pub settings_revision: SettingsRevision,
    pub policy_epoch: PolicyEpoch,
    pub required_capabilities: Set<SurfaceCapability>,
    pub capability_fingerprint: Sha256Digest,
    pub settings_receipt: OperationSettingsPreparationReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationSettingsPreparation {
    UseCurrent {
        expected_settings_revision: SettingsRevision,
        expected_policy_epoch: PolicyEpoch,
    },
    ApplyThreadOverridesBeforeRequested {
        expected_settings_revision: SettingsRevision,
        expected_policy_epoch: PolicyEpoch,
        patches: NonEmptyVec<RuntimeSettingsPatch>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationSettingsPreparationReceipt {
    Current {
        settings_revision: SettingsRevision,
        policy_epoch: PolicyEpoch,
    },
    ThreadOverridesCommitted {
        previous_settings_revision: SettingsRevision,
        settings_revision: SettingsRevision,
        policy_epoch: PolicyEpoch,
        patches_digest: Sha256Digest,
        host_commit_id: SurfaceCommitId,
        thread_settings_cursor: SurfaceCursor,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Replayability {
    Replayable {
        capsule_digest: Sha256Digest,
        request: Option<SurfaceInputRequest>,
        request_digest: Option<Sha256Digest>,
        cwd: CanonicalPath,
        workspace_roots: Vec<CanonicalPath>,
        settings_revision: SettingsRevision,
        policy_epoch: PolicyEpoch,
        tool_schema_digest: Sha256Digest,
    },
    NonReplayable {
        reason: NonReplayableReason,
        live_capsule: LiveOperationCapsule,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LiveOperationCapsule {
    Available { incarnation: SurfaceIncarnation },
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StaleLiveCapsuleDescriptor {
    Stale { incarnation: SurfaceIncarnation },
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LiveCapsuleStatus {
    Current,
    NotCurrent {
        descriptor: StaleLiveCapsuleDescriptor,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReplayabilityClass {
    Replayable,
    NonReplayable(LiveCapsuleStatus),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinalizerPhaseClass {
    Admitted,
    SuspendedResumeStarting { generation_id: SurfaceGenerationId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AdmittedInput {
    PendingUser {
        item_id: SurfaceItemId,
        presentation: SurfaceInputPresentation,
        correlation_id: SurfaceInputCorrelationId,
    },
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReservationLease {
    pub lease_id: SurfaceAdmissionLeaseId,
    pub operation_id: SurfaceOperationId,
    pub reservation_sequence: SequenceNumber,
    pub issuing_host_incarnation: HostIncarnation,
    pub issued_at: MonotonicInstant,
    duration: DurationMillis,
}

impl ReservationLease {
    pub(crate) fn new(
        lease_id: SurfaceAdmissionLeaseId,
        operation_id: SurfaceOperationId,
        reservation_sequence: SequenceNumber,
        issuing_host_incarnation: HostIncarnation,
        issued_at: MonotonicInstant,
    ) -> Self {
        Self {
            lease_id,
            operation_id,
            reservation_sequence,
            issuing_host_incarnation,
            issued_at,
            duration: DurationMillis::new(SURFACE_RESERVATION_LEASE_MS),
        }
    }

    pub const fn duration(&self) -> DurationMillis {
        self.duration
    }
}

impl<'de> Deserialize<'de> for ReservationLease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireReservationLease {
            lease_id: SurfaceAdmissionLeaseId,
            operation_id: SurfaceOperationId,
            reservation_sequence: SequenceNumber,
            issuing_host_incarnation: HostIncarnation,
            issued_at: MonotonicInstant,
            duration: DurationMillis,
        }

        let wire = WireReservationLease::deserialize(deserializer)?;
        if wire.duration.get() != SURFACE_RESERVATION_LEASE_MS {
            return Err(serde::de::Error::custom(
                "reservation lease duration must be exactly 30000 milliseconds",
            ));
        }
        Ok(Self::new(
            wire.lease_id,
            wire.operation_id,
            wire.reservation_sequence,
            wire.issuing_host_incarnation,
            wire.issued_at,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationPhase {
    Requested,
    Admitted,
    Suspended {
        cause: SuspensionCause,
    },
    Finalizing {
        finalize_intent_id: SurfaceFinalizeIntentId,
    },
    FinalizingDegraded {
        finalize_intent_id: SurfaceFinalizeIntentId,
    },
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationPhase {
    Reserved,
    Started,
    Transferred,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TerminalizationCause {
    UserCancel,
    GoalPause,
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SuspensionCause {
    Interrupted { generation_id: SurfaceGenerationId },
    RecoveryRequired { generation_id: SurfaceGenerationId },
    ProviderSuspended { generation_id: SurfaceGenerationId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SuspendedFinalizationCause {
    Terminalization(TerminalizationCause),
    ResumeStartCommitFailure {
        message: SafeDiagnosticText,
    },
    RecoveryAbortNonReplayable {
        last_generation: SurfaceGenerationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PendingControlIntent {
    Interrupt {
        generation_fence: SurfaceOperationFence,
    },
    Terminalize {
        operation_id: SurfaceOperationId,
        cause: TerminalizationCause,
    },
    ResumeStarting {
        generation_fence: SurfaceOperationFence,
    },
    ResumeAfterInterruptedStop {
        generation_fence: SurfaceOperationFence,
    },
    BackgroundOnStart {
        operation_id: SurfaceOperationId,
        reservation_sequence: SequenceNumber,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceShutdownReason {
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NotStartedReason {
    ReservationExpired,
    Cancelled { cause: TerminalizationCause },
    Interrupted,
    RuntimeRestart,
    StartCommitFailure { message: SafeDiagnosticText },
    MissingLiveInputCapsule,
    AdmissionRejected { reason: AdmissionRejectionReason },
    Shutdown { reason: SurfaceShutdownReason },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationStopReason {
    Completed {
        status: GenerationCompletionStatus,
    },
    Cancelled {
        cause: TerminalizationCause,
    },
    InterruptedResumable,
    ProviderSuspended,
    RuntimeRestart,
    ProjectionFailure {
        message: SafeDiagnosticText,
    },
    ExecutionFailed {
        class: GenerationExecutionFailureClass,
        message: SafeDiagnosticText,
    },
    Panicked {
        message: SafeDiagnosticText,
    },
    NotStarted {
        reason: NotStartedReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationCompletionStatus {
    Success,
    VerificationFailed { message: SafeDiagnosticText },
    BudgetExhausted { budget: OperationBudget },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationExecutionFailureClass {
    Provider,
    Tool,
    Hook,
    Workflow,
    InputResolution,
    ClientCapabilityUnavailable,
    LegacyApprovalRequired,
    RuntimeInvariant,
    ExternalEffectAmbiguous,
    RemoteResourceCleanupAmbiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GoalOuterTurnOrigin {
    User,
    Resume,
    Continuation,
    WorkflowNotification,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationAttempt {
    Initial,
    RecoveryReplacement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceGoalGenerationIdentity {
    pub goal_id: SurfaceGoalId,
    pub goal_run_id: SurfaceGoalRunId,
    pub operation_fence: SurfaceOperationFence,
    pub goal_outer_turn_id: SurfaceGoalOuterTurnId,
    pub logical_turn_id: SurfaceTurnId,
    pub canonical_input_item_id: SurfaceItemId,
    pub outer_turn_origin: GoalOuterTurnOrigin,
    pub attempt: GenerationAttempt,
    pub predecessor_fence: Option<SurfaceOperationFence>,
    pub objective_revision: GoalObjectiveRevision,
    pub outer_turn_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GenerationStartedWitness {
    pub started_commit_id: SurfaceCommitId,
    pub settings_revision: SettingsRevision,
    pub policy_epoch: PolicyEpoch,
    pub durable_replayability_digest: Sha256Digest,
    pub capability_fingerprint: Sha256Digest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InputResolutionErrorCode {
    MalformedLegacyTarget,
    StaleCatalog,
    KindMismatch,
    OutsideWorkspace,
    TargetNotFound,
    ReadFailed,
    UnsupportedMime,
    RuntimeUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInputPresentation {
    Visible { text: DisplayText },
    Redacted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceResolvedInputFact {
    Replayable {
        input: SurfaceInput,
        request_digest: Sha256Digest,
    },
    NonReplayable {
        presentation: SurfaceInputPresentation,
        live_capsule_incarnation: SurfaceIncarnation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GenerationInputState {
    NotApplicable,
    Pending {
        input_item_id: SurfaceItemId,
        presentation: SurfaceInputPresentation,
        correlation_id: SurfaceInputCorrelationId,
    },
    Resolved {
        input_item_id: SurfaceItemId,
        fact: SurfaceResolvedInputFact,
    },
    Failed {
        input_item_id: SurfaceItemId,
        code: InputResolutionErrorCode,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub fence: SurfaceOperationFence,
    pub logical_turn_id: SurfaceTurnId,
    pub input: GenerationInputState,
    pub predecessor: Option<SurfaceOperationFence>,
    pub attempt: GenerationAttempt,
    pub goal_identity: Option<SurfaceGoalGenerationIdentity>,
    pub replayability: Replayability,
    pub required_capabilities: Set<SurfaceCapability>,
    pub capability_fingerprint: Sha256Digest,
    pub phase: GenerationPhase,
    pub started_witness: Option<GenerationStartedWitness>,
    pub stop_reason: Option<GenerationStopReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceTaskRunningStatus {
    Running,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAgentLoopTurn {
    pub turn_id: SurfaceTurnId,
    pub fence: SurfaceOperationFence,
    pub ordinal: u32,
    pub task_id: SurfaceTaskId,
    pub task_status: SurfaceTaskRunningStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation_id: SurfaceOperationId,
    pub request_id: SurfaceRequestId,
    pub intent: OperationIntent,
    pub phase: OperationPhase,
    pub reservation: ReservationLease,
    pub ready_for_admission: bool,
    pub initial_logical_turn_id: Option<SurfaceTurnId>,
    pub initial_input_item_id: Option<SurfaceItemId>,
    pub generations: Vec<GenerationRecord>,
    pub agent_loop_turns: Vec<SurfaceAgentLoopTurn>,
    pub pending_control: Option<PendingControlIntent>,
    pub finalization: Option<OperationFinalizationRecord>,
    pub terminal: Option<OperationTerminalRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TurnRequestBudgetScope {
    AgentLoop,
    Subagent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationBudget {
    ModelTokens {
        limit: Option<u64>,
        observed: Option<u64>,
    },
    TurnRequests {
        scope: TurnRequestBudgetScope,
        limit: u64,
        observed: u64,
    },
    ToolCalls {
        limit: u64,
        observed: u64,
    },
    WallTimeMs {
        limit: u64,
        observed: u64,
    },
    GoalTokenBudget {
        goal_id: SurfaceGoalId,
        limit: i64,
        observed: i64,
    },
    WorkflowTokenBudget {
        workflow_run_id: SurfaceWorkflowRunId,
        limit: u64,
        observed: u64,
    },
    MonetaryBudgetUsdMicros {
        limit: u64,
        observed: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AdmissionRejectionReason {
    ConfigurationConflict,
    PolicyConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NotAdmittedReason {
    CancelledBeforeAdmission,
    ReservationExpired,
    ConfigurationConflict,
    PolicyConflict,
    RuntimeRestart,
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CancelReason {
    User,
    GoalPause,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FailureClass {
    Provider,
    Tool,
    Hook,
    Workflow,
    Verification,
    InputResolution,
    ClientCapabilityUnavailable,
    LegacyApprovalRequired,
    RuntimeInvariant,
    Persistence,
    ExternalEffectAmbiguous,
    RemoteResourceCleanupAmbiguous,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationTerminal {
    NotAdmitted {
        reason: NotAdmittedReason,
    },
    Succeeded {
        usage: UsageTotals,
    },
    Cancelled {
        reason: CancelReason,
    },
    BudgetExhausted {
        budget: OperationBudget,
    },
    Failed {
        class: FailureClass,
        message: SafeDiagnosticText,
    },
    Panicked {
        message: SafeDiagnosticText,
    },
    JoinFailed {
        message: SafeDiagnosticText,
    },
    AbortedByRuntimeRestart {
        last_generation: SurfaceGenerationId,
    },
    Shutdown {
        reason: SurfaceShutdownReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub estimated_cost_usd_micros: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationTerminalRecord {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal: OperationTerminal,
    pub usage: UsageTotals,
    pub source_diagnostic_digest: Option<Sha256Digest>,
    pub settlement_receipts: Vec<SurfaceSettlementReceipt>,
    #[serde(default)]
    pub completion_proof: SurfaceOperationCompletionProof,
    pub committed_at: UnixMillis,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSettlementReceipt {
    pub settlement_id: SurfaceSettlementId,
    pub receipt_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FinalizationStartedAtCursor {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub event_id: SurfaceEventId,
    pub cursor: SurfaceCursor,
    pub commit_class: CommitClass,
    pub batch_digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationFinalizationRecord {
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub terminal_commit_id: SurfaceCommitId,
    pub started_at: FinalizationStartedAtCursor,
    pub selected_cause: OperationFinalizationCause,
    pub suspended_cause: Option<SuspendedFinalizationCause>,
    pub expected_settlements: Vec<SurfaceSettlementId>,
    pub settled: Vec<SurfaceSettlementReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinalizationDegradedCause {
    MissingFinalization {
        terminal_commit_id: SurfaceCommitId,
        missing_settlements: NonEmptyVec<SurfaceSettlementId>,
        missing_set_digest: Sha256Digest,
    },
    TerminalProjectionPending {
        terminal_commit_id: SurfaceCommitId,
        terminal_event_id: SurfaceEventId,
        durable_revision: DurableRevision,
        terminal_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReservationFinalizerReason {
    ReservationExpired,
    AdmissionRejected { reason: AdmissionRejectionReason },
    CancelledBeforeAdmission,
    RuntimeRestart,
    HostShutdown,
    ThreadClose,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReservationFinalizerSource {
    pub reason: ReservationFinalizerReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationJoinSettlementSource {
    pub operation_id: SurfaceOperationId,
    pub finalize_intent_id: SurfaceFinalizeIntentId,
    pub settlement_id: SurfaceSettlementId,
    pub settlement_receipt_digest: Sha256Digest,
    pub message: SafeDiagnosticText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationFinalizerSource {
    GenerationStop {
        reason: GenerationStopReason,
    },
    Reservation {
        source: ReservationFinalizerSource,
    },
    OperationJoinSettlement {
        source: OperationJoinSettlementSource,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationFinalizationCause {
    Terminalization(TerminalizationCause),
    GenerationStop(GenerationStopReason),
    Reservation(ReservationFinalizerReason),
    OperationJoinSettlement(OperationJoinSettlementSource),
    Suspended(SuspendedFinalizationCause),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MaterializationCause {
    SameProcessProjectionReset {
        retained_incarnation: SurfaceIncarnation,
    },
    ColdOwnerTakeover {
        new_incarnation: SurfaceIncarnation,
        new_owner_epoch: ThreadOwnerEpoch,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceReasoningEffort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceApprovalMode {
    Suggest,
    AutoEdit,
    FullAuto,
    Plan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePermissionDecision {
    Allow,
    Prompt,
    Deny,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceNetworkDomainAccess {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceActivePermissionProfile {
    pub id: NonEmptyText,
    pub extends: Option<NonEmptyText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionRule {
    pub tool: NonEmptyText,
    pub pattern: NonEmptyText,
    pub decision: SurfacePermissionDecision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionRuleSet {
    pub ordered_rules: Vec<SurfacePermissionRule>,
    pub digest: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceAdditionalWorkingDirectory {
    pub path: CanonicalPath,
    pub source: NonEmptyText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceNetworkDomainPermission {
    pub domain: CanonicalDomainName,
    pub access: SurfaceNetworkDomainAccess,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceNetworkPermissions {
    pub enabled: Option<bool>,
    pub domains: Vec<SurfaceNetworkDomainPermission>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRuntimeSettings {
    pub model: NonEmptyText,
    pub reasoning_effort: SurfaceReasoningEffort,
    pub approval_mode: SurfaceApprovalMode,
    pub cwd: CanonicalPath,
    pub workspace_roots: Vec<CanonicalPath>,
    pub active_permission_profile: Option<SurfaceActivePermissionProfile>,
    pub permission_rules: SurfacePermissionRuleSet,
    pub additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
    pub network_permissions: SurfaceNetworkPermissions,
    pub policy_epoch: PolicyEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSettingsDestination {
    Session,
    UserSettings,
    ProjectSettings,
    LocalSettings,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionRuleSelector {
    pub tool: NonEmptyText,
    pub pattern: Option<NonEmptyText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePermissionUpdate {
    AddRules {
        destination: SurfaceSettingsDestination,
        decision: SurfacePermissionDecision,
        rules: NonEmptyVec<SurfacePermissionRuleSelector>,
    },
    ReplaceRules {
        destination: SurfaceSettingsDestination,
        decision: SurfacePermissionDecision,
        rules: Vec<SurfacePermissionRuleSelector>,
    },
    RemoveRules {
        destination: SurfaceSettingsDestination,
        decision: SurfacePermissionDecision,
        rules: NonEmptyVec<SurfacePermissionRuleSelector>,
    },
    SetMode {
        destination: SurfaceSettingsDestination,
        mode: SurfaceApprovalMode,
    },
    AddDirectories {
        destination: SurfaceSettingsDestination,
        directories: NonEmptyVec<SurfaceAdditionalWorkingDirectory>,
    },
    RemoveDirectories {
        destination: SurfaceSettingsDestination,
        paths: NonEmptyVec<CanonicalPath>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeSettingsPatch {
    SetModel {
        model: NonEmptyText,
    },
    SetReasoning {
        effort: SurfaceReasoningEffort,
    },
    SetApprovalMode {
        mode: SurfaceApprovalMode,
    },
    SetCwd {
        cwd: CanonicalPath,
    },
    SetWorkspaceRoots {
        roots: Vec<CanonicalPath>,
    },
    SetActivePermissionProfile {
        profile: Option<SurfaceActivePermissionProfile>,
    },
    ReplacePermissionRules {
        rules: Vec<SurfacePermissionRule>,
    },
    ReplaceAdditionalWorkingDirectories {
        directories: Vec<SurfaceAdditionalWorkingDirectory>,
    },
    ReplaceNetworkPermissions {
        permissions: SurfaceNetworkPermissions,
    },
    ApplyPermissionUpdate {
        update: SurfacePermissionUpdate,
    },
}
