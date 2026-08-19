use super::identity::{
    CanonicalBackgroundFenceV1, CanonicalPath, CanonicalTaskFenceV1, Denied, DisplayText,
    FiniteF64, HostIncarnation, HostMonotonicClockId, InteractionRevision, MonotonicInstant,
    NonEmptySet, NonEmptyText, NonEmptyVec, OpaqueToken, PolicyEpoch, ResponseRouteEpoch,
    Sha256Digest, SurfaceAttachmentId, SurfaceBackgroundFence, SurfaceConnectionId,
    SurfaceIncarnation, SurfaceInteractionId, SurfaceOperationFence, SurfaceOperationId,
    SurfaceRequestId, SurfaceResponseGrantToken, SurfaceResponseId, SurfaceResponseReceiptId,
    SurfaceResponseToken, SurfaceSettlementId, SurfaceTaskFence, SurfaceThreadId,
    SurfaceToolCallId, SurfaceTurnId, SurfaceValueError, UnixMillis, UuidV7,
    canonical_background_fence_v1, canonical_task_fence_v1,
};
use super::operation::CancelReason;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum SurfaceInteractionKind {
    ToolApproval,
    PermissionRequest,
    UserInput,
    McpElicitation,
    BackgroundApproval,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InteractionExpiryDeadline {
    pub issuing_host_incarnation: HostIncarnation,
    pub expires_at: MonotonicInstant,
    pub observed_expires_at: Option<UnixMillis>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionExpiryAuthorityFailure {
    ClockIdMismatch {
        expected: HostMonotonicClockId,
        observed: HostMonotonicClockId,
    },
    TickArithmeticOverflow {
        clock_id: HostMonotonicClockId,
    },
    IssuingHostLost {
        clock_id: HostMonotonicClockId,
        issuing_host_incarnation: HostIncarnation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionUnavailableDisposition {
    FailOperation,
    AwaitCapableAttachment {
        deadline: InteractionExpiryDeadline,
    },
    /// A typed ToolApproval checkpoint may be re-parked by a cold runtime
    /// owner. The encoded capsule remains opaque to surface adapters.
    RestartableToolApproval {
        capsule: Vec<u8>,
    },
    /// A typed PermissionRequest checkpoint may be re-parked only when its
    /// capsule proves the bound tool has not crossed an external side-effect
    /// boundary. The encoded capsule remains opaque to surface adapters.
    RestartablePermissionRequest {
        capsule: Vec<u8>,
    },
    /// A typed UserInput checkpoint restarts by injecting the durable answer
    /// into a fresh continuation turn; it never restores the original waiter.
    RestartableUserInput {
        capsule: Vec<u8>,
    },
    /// A typed MCP elicitation checkpoint restarts by injecting the accepted
    /// content into a fresh continuation turn; it never restores the MCP call
    /// stack that created the request.
    RestartableMcpElicitation {
        capsule: Vec<u8>,
    },
}

impl InteractionUnavailableDisposition {
    pub(crate) fn restartable_tool_approval(
        capsule: &DurableInteractionContinuationCapsule,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        Ok(Self::RestartableToolApproval {
            capsule: capsule.encode()?,
        })
    }

    pub(crate) fn restartable_tool_approval_capsule(
        &self,
    ) -> Result<
        Option<DurableInteractionContinuationCapsule>,
        DurableInteractionContinuationCapsuleError,
    > {
        match self {
            Self::RestartableToolApproval { capsule } => {
                DurableInteractionContinuationCapsule::decode(capsule).map(Some)
            }
            Self::FailOperation
            | Self::AwaitCapableAttachment { .. }
            | Self::RestartablePermissionRequest { .. }
            | Self::RestartableUserInput { .. }
            | Self::RestartableMcpElicitation { .. } => Ok(None),
        }
    }

    pub(crate) fn restartable_permission_request(
        capsule: &DurableInteractionContinuationCapsule,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        Ok(Self::RestartablePermissionRequest {
            capsule: capsule.encode()?,
        })
    }

    pub(crate) fn restartable_permission_request_capsule(
        &self,
    ) -> Result<
        Option<DurableInteractionContinuationCapsule>,
        DurableInteractionContinuationCapsuleError,
    > {
        match self {
            Self::RestartablePermissionRequest { capsule } => {
                DurableInteractionContinuationCapsule::decode(capsule).map(Some)
            }
            Self::FailOperation
            | Self::AwaitCapableAttachment { .. }
            | Self::RestartableToolApproval { .. }
            | Self::RestartableUserInput { .. }
            | Self::RestartableMcpElicitation { .. } => Ok(None),
        }
    }

    pub(crate) fn restartable_continuation_turn(
        capsule: &DurableInteractionContinuationCapsule,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        let capsule = capsule.encode()?;
        match DurableInteractionContinuationCapsule::decode(&capsule)?.kind() {
            SurfaceInteractionKind::UserInput => Ok(Self::RestartableUserInput { capsule }),
            SurfaceInteractionKind::McpElicitation => {
                Ok(Self::RestartableMcpElicitation { capsule })
            }
            kind => {
                Err(DurableInteractionContinuationCapsuleError::UnsupportedInteractionKind { kind })
            }
        }
    }

    pub(crate) fn restartable_continuation_turn_capsule(
        &self,
    ) -> Result<
        Option<DurableInteractionContinuationCapsule>,
        DurableInteractionContinuationCapsuleError,
    > {
        match self {
            Self::RestartableUserInput { capsule }
            | Self::RestartableMcpElicitation { capsule } => {
                DurableInteractionContinuationCapsule::decode(capsule).map(Some)
            }
            Self::FailOperation
            | Self::AwaitCapableAttachment { .. }
            | Self::RestartableToolApproval { .. }
            | Self::RestartablePermissionRequest { .. } => Ok(None),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerInteractionResponseRoute {
    Unassigned {
        epoch: ResponseRouteEpoch,
    },
    Exclusive {
        epoch: ResponseRouteEpoch,
        attachment_id: SurfaceAttachmentId,
        grant_token: SurfaceResponseGrantToken,
    },
    SharedFirstCommitWins {
        epoch: ResponseRouteEpoch,
        grants: NonEmptyVec<(SurfaceAttachmentId, SurfaceResponseGrantToken)>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInteractionRoute {
    Unassigned {
        epoch: ResponseRouteEpoch,
    },
    Exclusive {
        epoch: ResponseRouteEpoch,
        attachment_id: SurfaceAttachmentId,
    },
    SharedFirstCommitWins {
        epoch: ResponseRouteEpoch,
        attachments: NonEmptySet<SurfaceAttachmentId>,
    },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthorityFingerprint {
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

#[derive(Serialize)]
pub(super) struct CanonicalAuthorityFingerprintV1<'a> {
    operation_id: &'a SurfaceOperationId,
    request_digest: &'a Sha256Digest,
    tool_digest: &'a Sha256Digest,
    cwd: &'a CanonicalPath,
    workspace_roots_digest: &'a Sha256Digest,
    policy_epoch: PolicyEpoch,
    executable_generation: &'a Sha256Digest,
    artifact_generation: &'a Sha256Digest,
    capability_digest: &'a Sha256Digest,
}

fn canonical_authority_fingerprint_v1(
    authority: &AuthorityFingerprint,
) -> CanonicalAuthorityFingerprintV1<'_> {
    CanonicalAuthorityFingerprintV1 {
        operation_id: &authority.operation_id,
        request_digest: &authority.request_digest,
        tool_digest: &authority.tool_digest,
        cwd: &authority.cwd,
        workspace_roots_digest: &authority.workspace_roots_digest,
        policy_epoch: authority.policy_epoch,
        executable_generation: &authority.executable_generation,
        artifact_generation: &authority.artifact_generation,
        capability_digest: &authority.capability_digest,
    }
}

#[allow(dead_code)]
impl AuthorityFingerprint {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        operation_id: SurfaceOperationId,
        request_digest: Sha256Digest,
        tool_digest: Sha256Digest,
        cwd: CanonicalPath,
        workspace_roots_digest: Sha256Digest,
        policy_epoch: PolicyEpoch,
        executable_generation: Sha256Digest,
        artifact_generation: Sha256Digest,
        capability_digest: Sha256Digest,
    ) -> Self {
        Self {
            operation_id,
            request_digest,
            tool_digest,
            cwd,
            workspace_roots_digest,
            policy_epoch,
            executable_generation,
            artifact_generation,
            capability_digest,
        }
    }

    pub fn operation_id(&self) -> &SurfaceOperationId {
        &self.operation_id
    }

    pub fn request_digest(&self) -> &Sha256Digest {
        &self.request_digest
    }

    pub fn tool_digest(&self) -> &Sha256Digest {
        &self.tool_digest
    }

    pub fn cwd(&self) -> &CanonicalPath {
        &self.cwd
    }

    pub fn workspace_roots_digest(&self) -> &Sha256Digest {
        &self.workspace_roots_digest
    }

    pub const fn policy_epoch(&self) -> PolicyEpoch {
        self.policy_epoch
    }

    pub fn executable_generation(&self) -> &Sha256Digest {
        &self.executable_generation
    }

    pub fn artifact_generation(&self) -> &Sha256Digest {
        &self.artifact_generation
    }

    pub fn capability_digest(&self) -> &Sha256Digest {
        &self.capability_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfacePermissionPathLabel(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfacePermissionDomainPattern(pub DisplayText);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceFileSystemPermissionProfile {
    pub read: Option<Vec<SurfacePermissionPathLabel>>,
    pub write: Option<Vec<SurfacePermissionPathLabel>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceShellPermissionProfile {
    pub unsandboxed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceAllowDeny {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionNetworkProfile {
    pub enabled: Option<bool>,
    pub domains: Vec<(SurfacePermissionDomainPattern, SurfaceAllowDeny)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfacePermissionProfile {
    pub file_system: Option<SurfaceFileSystemPermissionProfile>,
    pub network: Option<SurfacePermissionNetworkProfile>,
    pub shell: Option<SurfaceShellPermissionProfile>,
}

impl SurfacePermissionProfile {
    pub const fn empty() -> Self {
        Self {
            file_system: None,
            network: None,
            shell: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PermissionGrantScope {
    Turn,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NegativeI64(i64);

impl NegativeI64 {
    pub fn try_new(value: i64) -> Result<Self, SurfaceValueError> {
        if value >= 0 {
            return Err(SurfaceValueError::NonCanonical);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for NegativeI64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(i64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSchemaInteger {
    Negative(NegativeI64),
    NonNegative(u64),
}

impl SurfaceSchemaInteger {
    pub fn try_negative(value: i64) -> Result<Self, SurfaceValueError> {
        NegativeI64::try_new(value).map(Self::Negative)
    }

    pub const fn non_negative(value: u64) -> Self {
        Self::NonNegative(value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceSchema {
    String {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        enum_values: Vec<DisplayText>,
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    Integer {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        minimum: Option<SurfaceSchemaInteger>,
        maximum: Option<SurfaceSchemaInteger>,
        enum_values: Vec<SurfaceSchemaInteger>,
    },
    Number {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        minimum: Option<FiniteF64>,
        maximum: Option<FiniteF64>,
    },
    Boolean {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
    },
    Array {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        items: Box<SurfaceSchema>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    Object {
        title: Option<DisplayText>,
        description: Option<DisplayText>,
        properties: Vec<SurfaceSchemaProperty>,
        additional_properties: Denied,
    },
    Unsupported {
        schema_digest: Sha256Digest,
        unsupported_keywords: NonEmptyVec<NonEmptyText>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceSchemaProperty {
    pub name: DisplayText,
    pub required: bool,
    pub schema: Box<SurfaceSchema>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceDataValue {
    Null,
    Boolean(bool),
    Integer(NegativeI64),
    Unsigned(u64),
    Number(FiniteF64),
    String(DisplayText),
    Array(Vec<SurfaceDataValue>),
    Object(Vec<SurfaceDataProperty>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SurfaceDataProperty {
    pub name: DisplayText,
    pub value: Box<SurfaceDataValue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceToolAction {
    Read,
    Write,
    Network,
    Agent,
    Shell,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceToolRequest {
    pub tool_call_id: SurfaceToolCallId,
    pub source_response_id: Option<UuidV7>,
    pub turn_id: SurfaceTurnId,
    pub name: NonEmptyText,
    pub action: SurfaceToolAction,
    pub target: Option<DisplayText>,
    pub raw_arguments: DisplayText,
    pub arguments_digest: Sha256Digest,
}

#[derive(Clone, PartialEq)]
pub enum SurfaceInteractionRequest {
    ToolApproval {
        tool: SurfaceToolRequest,
        description: DisplayText,
        preview: Option<DisplayText>,
        authority: AuthorityFingerprint,
    },
    PermissionRequest {
        tool_call_id: SurfaceToolCallId,
        reason: Option<DisplayText>,
        permissions: SurfacePermissionProfile,
        authority: AuthorityFingerprint,
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
        task: SurfaceTaskFence,
        tool: SurfaceToolRequest,
        authority: AuthorityFingerprint,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpElicitationRequest {
    Form {
        requested_schema: Option<SurfaceDataValue>,
        supported_schema: Option<SurfaceSchema>,
    },
    Url {
        raw_url: Option<DisplayText>,
        requested_schema: Option<SurfaceDataValue>,
    },
}

const LEGACY_DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION: u8 = 1;
pub(crate) const DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ToolInvocationCheckpoint {
    BeforeInvocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PermissionRetryCheckpoint {
    PreSideEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum DurableInteractionContinuationDisposition {
    Restartable,
    Executing,
    Unsafe,
    Unsupported,
}

/// A durable logical tool invocation that may be dispatched only from the
/// `BeforeInvocation` checkpoint after approval recovery. Execution-only
/// dependencies are rebuilt from the owning thread and checked separately by
/// the capsule execution-context fingerprint.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolInvocationIntent {
    invocation_id: SurfaceToolCallId,
    request: SurfaceToolRequest,
    authority: AuthorityFingerprint,
    checkpoint: ToolInvocationCheckpoint,
}

impl ToolInvocationIntent {
    /// Freeze a complete tool request and authority before any invocation is
    /// started. This performs no dispatch, persistence, or external effect.
    pub fn before_invocation(request: SurfaceToolRequest, authority: AuthorityFingerprint) -> Self {
        Self {
            invocation_id: request.tool_call_id.clone(),
            request,
            authority,
            checkpoint: ToolInvocationCheckpoint::BeforeInvocation,
        }
    }

    pub fn invocation_id(&self) -> &SurfaceToolCallId {
        &self.invocation_id
    }

    pub fn request(&self) -> &SurfaceToolRequest {
        &self.request
    }

    pub fn authority(&self) -> &AuthorityFingerprint {
        &self.authority
    }

    pub const fn checkpoint(&self) -> ToolInvocationCheckpoint {
        self.checkpoint
    }
}

/// Serializable permission state that must be restored before retrying the
/// bound tool invocation. It deliberately contains no process-local handles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PermissionRetryOverlay {
    pub additional_working_directories: Vec<CanonicalPath>,
    pub metadata_writable_directories: Vec<CanonicalPath>,
    pub network_domain_permissions: Vec<(SurfacePermissionDomainPattern, SurfaceAllowDeny)>,
    pub strict_auto_review: bool,
}

impl PermissionRetryOverlay {
    pub const fn empty() -> Self {
        Self {
            additional_working_directories: Vec::new(),
            metadata_writable_directories: Vec::new(),
            network_domain_permissions: Vec::new(),
            strict_auto_review: false,
        }
    }
}

/// A complete pre-side-effect retry intent. The stable invocation id must
/// match both the serialized tool request and the permission request.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PermissionRetryIntent {
    invocation_id: SurfaceToolCallId,
    tool: SurfaceToolRequest,
    requested_permissions: SurfacePermissionProfile,
    permission_overlay: PermissionRetryOverlay,
    authority: AuthorityFingerprint,
    checkpoint: PermissionRetryCheckpoint,
}

impl PermissionRetryIntent {
    /// Freeze the exact tool, requested permission, and existing overlay at a
    /// proven pre-side-effect denial point. The owning capsule separately
    /// binds the thread-owned execution context by fingerprint.
    pub fn pre_side_effect(
        tool: SurfaceToolRequest,
        requested_permissions: SurfacePermissionProfile,
        permission_overlay: PermissionRetryOverlay,
        authority: AuthorityFingerprint,
    ) -> Self {
        Self {
            invocation_id: tool.tool_call_id.clone(),
            tool,
            requested_permissions,
            permission_overlay,
            authority,
            checkpoint: PermissionRetryCheckpoint::PreSideEffect,
        }
    }

    pub fn invocation_id(&self) -> &SurfaceToolCallId {
        &self.invocation_id
    }

    pub fn tool(&self) -> &SurfaceToolRequest {
        &self.tool
    }

    pub fn requested_permissions(&self) -> &SurfacePermissionProfile {
        &self.requested_permissions
    }

    pub fn permission_overlay(&self) -> &PermissionRetryOverlay {
        &self.permission_overlay
    }

    pub fn authority(&self) -> &AuthorityFingerprint {
        &self.authority
    }

    pub const fn checkpoint(&self) -> PermissionRetryCheckpoint {
        self.checkpoint
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ContinuationTurnAnswerType {
    UserText,
    McpContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ContinuationTurnContextKind {
    #[serde(alias = "PinnedSystemTaskNotification")]
    PinnedUserTaskNotification,
}

/// The answer is rendered through this durable, typed template and delivered
/// by the existing steer/continuation-turn context path. It contains no
/// process-local sender, waiter, closure, or worker handle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContinuationTurnAnswerInjection {
    context_kind: ContinuationTurnContextKind,
    answer_type: ContinuationTurnAnswerType,
    template: NonEmptyText,
}

impl ContinuationTurnAnswerInjection {
    fn user_input() -> Self {
        Self {
            context_kind: ContinuationTurnContextKind::PinnedUserTaskNotification,
            answer_type: ContinuationTurnAnswerType::UserText,
            template: NonEmptyText::try_new(
                "[Interaction continuation]\nA durable UserInput request was answered. Continue from the answer below without resuming the suspended request call stack."
                    .to_string(),
            )
            .expect("fixed continuation template is non-empty"),
        }
    }

    fn mcp_elicitation() -> Self {
        Self {
            context_kind: ContinuationTurnContextKind::PinnedUserTaskNotification,
            answer_type: ContinuationTurnAnswerType::McpContent,
            template: NonEmptyText::try_new(
                "[Interaction continuation]\nA durable MCP elicitation request was accepted. Continue from the content below without resuming the suspended MCP call stack."
                    .to_string(),
            )
            .expect("fixed continuation template is non-empty"),
        }
    }

    pub const fn context_kind(&self) -> ContinuationTurnContextKind {
        self.context_kind
    }

    pub const fn answer_type(&self) -> ContinuationTurnAnswerType {
        self.answer_type
    }

    pub fn template(&self) -> &NonEmptyText {
        &self.template
    }
}

/// Durable prompt intent whose answer starts a new turn instead of resuming a
/// suspended Rust call stack. The capsule separately binds the original
/// thread/operation fence and execution-context fingerprint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ContinuationTurnIntent {
    UserInput {
        request_id: NonEmptyText,
        question: NonEmptyText,
        suggestions: Vec<DisplayText>,
        injection: ContinuationTurnAnswerInjection,
    },
    McpElicitation {
        server_name: NonEmptyText,
        server_request_id: NonEmptyText,
        message: DisplayText,
        request: SurfaceMcpElicitationRequest,
        injection: ContinuationTurnAnswerInjection,
    },
}

impl ContinuationTurnIntent {
    pub fn user_input(
        request_id: NonEmptyText,
        question: NonEmptyText,
        suggestions: Vec<DisplayText>,
    ) -> Self {
        Self::UserInput {
            request_id,
            question,
            suggestions,
            injection: ContinuationTurnAnswerInjection::user_input(),
        }
    }

    pub fn mcp_elicitation(
        server_name: NonEmptyText,
        server_request_id: NonEmptyText,
        message: DisplayText,
        request: SurfaceMcpElicitationRequest,
    ) -> Self {
        Self::McpElicitation {
            server_name,
            server_request_id,
            message,
            request,
            injection: ContinuationTurnAnswerInjection::mcp_elicitation(),
        }
    }

    pub const fn kind(&self) -> SurfaceInteractionKind {
        match self {
            Self::UserInput { .. } => SurfaceInteractionKind::UserInput,
            Self::McpElicitation { .. } => SurfaceInteractionKind::McpElicitation,
        }
    }

    pub fn to_surface_request(&self) -> SurfaceInteractionRequest {
        match self {
            Self::UserInput {
                question,
                suggestions,
                ..
            } => SurfaceInteractionRequest::UserInput {
                question: question.clone(),
                suggestions: suggestions.clone(),
            },
            Self::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
                ..
            } => SurfaceInteractionRequest::McpElicitation {
                server_name: server_name.clone(),
                server_request_id: server_request_id.clone(),
                message: message.clone(),
                request: request.clone(),
            },
        }
    }

    pub fn request_identity(&self) -> &NonEmptyText {
        match self {
            Self::UserInput { request_id, .. } => request_id,
            Self::McpElicitation {
                server_request_id, ..
            } => server_request_id,
        }
    }

    pub fn injection(&self) -> &ContinuationTurnAnswerInjection {
        match self {
            Self::UserInput { injection, .. } | Self::McpElicitation { injection, .. } => injection,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum DurableInteractionContinuationIntent {
    ToolInvocation(ToolInvocationIntent),
    PermissionRetry(PermissionRetryIntent),
    ContinuationTurn(ContinuationTurnIntent),
}

impl DurableInteractionContinuationIntent {
    pub const fn kind(&self) -> SurfaceInteractionKind {
        match self {
            Self::ToolInvocation(_) => SurfaceInteractionKind::ToolApproval,
            Self::PermissionRetry(_) => SurfaceInteractionKind::PermissionRequest,
            Self::ContinuationTurn(intent) => intent.kind(),
        }
    }
}

/// Versioned restart checkpoint for a pending interaction. It contains only
/// serializable request, fence, execution-context fingerprint, disposition,
/// and intent data; never a live waiter, channel, future, closure, worker, or
/// host call stack.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct DurableInteractionContinuationCapsule {
    version: u8,
    interaction_id: SurfaceInteractionId,
    fence: SurfaceOperationFence,
    request: DurableInteractionContinuationRequest,
    execution_context_fingerprint: Option<Sha256Digest>,
    disposition: DurableInteractionContinuationDisposition,
    intent: Option<DurableInteractionContinuationIntent>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum DurableInteractionContinuationRequest {
    ToolApproval {
        tool: SurfaceToolRequest,
        description: DisplayText,
        preview: Option<DisplayText>,
        authority: AuthorityFingerprint,
    },
    PermissionRequest {
        tool_call_id: SurfaceToolCallId,
        reason: Option<DisplayText>,
        permissions: SurfacePermissionProfile,
        authority: AuthorityFingerprint,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableInteractionContinuationCapsuleError {
    InvalidEncoding,
    UnsupportedVersion {
        observed_version: u8,
    },
    UnsupportedInteractionKind {
        kind: SurfaceInteractionKind,
    },
    AuthorityFenceMismatch {
        kind: SurfaceInteractionKind,
    },
    MissingPermissionRetryContext,
    MissingDisposition,
    MissingExecutionContextFingerprint {
        kind: SurfaceInteractionKind,
    },
    UnexpectedExecutionContextFingerprint {
        disposition: DurableInteractionContinuationDisposition,
    },
    ExecutionContextFingerprintMismatch,
    MissingRestartableIntent {
        kind: SurfaceInteractionKind,
    },
    UnexpectedIntentForDisposition {
        disposition: DurableInteractionContinuationDisposition,
    },
    NonRestartableDisposition {
        disposition: DurableInteractionContinuationDisposition,
    },
    IntentKindMismatch {
        request_kind: SurfaceInteractionKind,
        intent_kind: SurfaceInteractionKind,
    },
    InvocationIdMismatch {
        kind: SurfaceInteractionKind,
    },
    RequestIntentMismatch {
        kind: SurfaceInteractionKind,
    },
}

impl std::fmt::Display for DurableInteractionContinuationCapsuleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEncoding => {
                formatter.write_str("durable interaction continuation capsule encoding is invalid")
            }
            Self::UnsupportedVersion { observed_version } => write!(
                formatter,
                "unsupported durable interaction continuation capsule version {observed_version}"
            ),
            Self::UnsupportedInteractionKind { kind } => write!(
                formatter,
                "unsupported durable interaction continuation kind {kind:?}"
            ),
            Self::AuthorityFenceMismatch { kind } => write!(
                formatter,
                "durable interaction continuation authority does not match the {kind:?} operation fence"
            ),
            Self::MissingPermissionRetryContext => write!(
                formatter,
                "permission retry requires the complete tool invocation and permission overlay"
            ),
            Self::MissingDisposition => write!(
                formatter,
                "durable interaction continuation capsule is missing a restart disposition"
            ),
            Self::MissingExecutionContextFingerprint { kind } => write!(
                formatter,
                "restartable durable interaction continuation for {kind:?} is missing its execution-context fingerprint"
            ),
            Self::UnexpectedExecutionContextFingerprint { disposition } => write!(
                formatter,
                "durable interaction continuation with {disposition:?} disposition must not carry an execution-context fingerprint"
            ),
            Self::ExecutionContextFingerprintMismatch => write!(
                formatter,
                "durable interaction continuation execution context does not match the recovered thread runtime snapshot"
            ),
            Self::MissingRestartableIntent { kind } => write!(
                formatter,
                "restartable durable interaction continuation for {kind:?} is missing its intent"
            ),
            Self::UnexpectedIntentForDisposition { disposition } => write!(
                formatter,
                "durable interaction continuation with {disposition:?} disposition must not carry a restart intent"
            ),
            Self::NonRestartableDisposition { disposition } => write!(
                formatter,
                "durable interaction continuation is not restartable: {disposition:?}"
            ),
            Self::IntentKindMismatch {
                request_kind,
                intent_kind,
            } => write!(
                formatter,
                "durable interaction continuation request kind {request_kind:?} does not match intent kind {intent_kind:?}"
            ),
            Self::InvocationIdMismatch { kind } => write!(
                formatter,
                "durable interaction continuation invocation identity does not match the {kind:?} request"
            ),
            Self::RequestIntentMismatch { kind } => write!(
                formatter,
                "durable interaction continuation intent does not match the {kind:?} request"
            ),
        }
    }
}

impl std::error::Error for DurableInteractionContinuationCapsuleError {}

impl DurableInteractionContinuationCapsule {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, DurableInteractionContinuationCapsuleError> {
        serde_json::to_vec(self)
            .map_err(|_| DurableInteractionContinuationCapsuleError::InvalidEncoding)
    }

    pub(crate) fn decode(
        encoded: &[u8],
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        serde_json::from_slice(encoded)
            .map_err(|_| DurableInteractionContinuationCapsuleError::InvalidEncoding)
    }

    /// Function intent contract:
    ///
    /// - Input: the persisted interaction identity, owning operation fence,
    ///   typed interaction request, and opaque fingerprint of the thread-owned
    ///   runtime configuration/dependency snapshot.
    /// - Output: an immutable version-2 restartable checkpoint for tool
    ///   approval, user input, or MCP elicitation.
    /// - Errors: rejects permission requests without the complete retry
    ///   context, unsupported kinds, mismatched identities, or authority bound
    ///   to a different operation.
    /// - State changes: none; this function performs no I/O, persistence,
    ///   presentation, waiter creation, channel creation, or external effect.
    pub fn try_new(
        interaction_id: SurfaceInteractionId,
        fence: SurfaceOperationFence,
        request: SurfaceInteractionRequest,
        execution_context_fingerprint: Sha256Digest,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        let request = DurableInteractionContinuationRequest::try_from(request)?;
        let fallback_request_identity = NonEmptyText::try_new(format!(
            "interaction:{}",
            uuid::Uuid::from_bytes(*interaction_id.as_bytes())
        ))
        .expect("interaction identity is non-empty");
        let intent = match &request {
            DurableInteractionContinuationRequest::ToolApproval {
                tool, authority, ..
            } => DurableInteractionContinuationIntent::ToolInvocation(
                ToolInvocationIntent::before_invocation(tool.clone(), authority.clone()),
            ),
            DurableInteractionContinuationRequest::PermissionRequest { .. } => {
                return Err(
                    DurableInteractionContinuationCapsuleError::MissingPermissionRetryContext,
                );
            }
            DurableInteractionContinuationRequest::UserInput {
                question,
                suggestions,
            } => DurableInteractionContinuationIntent::ContinuationTurn(
                ContinuationTurnIntent::user_input(
                    fallback_request_identity,
                    question.clone(),
                    suggestions.clone(),
                ),
            ),
            DurableInteractionContinuationRequest::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            } => DurableInteractionContinuationIntent::ContinuationTurn(
                ContinuationTurnIntent::mcp_elicitation(
                    server_name.clone(),
                    server_request_id.clone(),
                    message.clone(),
                    request.clone(),
                ),
            ),
        };
        let capsule = Self {
            version: DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION,
            interaction_id,
            fence,
            request,
            execution_context_fingerprint: Some(execution_context_fingerprint),
            disposition: DurableInteractionContinuationDisposition::Restartable,
            intent: Some(intent),
        };
        capsule.validate()?;
        Ok(capsule)
    }

    /// Construct a restartable checkpoint from an explicitly captured intent.
    /// The request and intent must describe the same interaction and stable
    /// invocation identity. This function has no external side effects.
    pub fn try_new_restartable(
        interaction_id: SurfaceInteractionId,
        fence: SurfaceOperationFence,
        request: SurfaceInteractionRequest,
        execution_context_fingerprint: Sha256Digest,
        intent: DurableInteractionContinuationIntent,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        let capsule = Self {
            version: DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION,
            interaction_id,
            fence,
            request: DurableInteractionContinuationRequest::try_from(request)?,
            execution_context_fingerprint: Some(execution_context_fingerprint),
            disposition: DurableInteractionContinuationDisposition::Restartable,
            intent: Some(intent),
        };
        capsule.validate()?;
        Ok(capsule)
    }

    /// Construct a persisted fail-closed marker for an interaction that must
    /// not be restarted because it is executing, unsafe, or unsupported.
    pub fn try_new_fail_closed(
        interaction_id: SurfaceInteractionId,
        fence: SurfaceOperationFence,
        request: SurfaceInteractionRequest,
        disposition: DurableInteractionContinuationDisposition,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        let request = DurableInteractionContinuationRequest::try_from(request)?;
        if disposition == DurableInteractionContinuationDisposition::Restartable {
            return Err(
                DurableInteractionContinuationCapsuleError::MissingRestartableIntent {
                    kind: request.kind(),
                },
            );
        }
        let capsule = Self {
            version: DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION,
            interaction_id,
            fence,
            request,
            execution_context_fingerprint: None,
            disposition,
            intent: None,
        };
        capsule.validate_structure()?;
        Ok(capsule)
    }

    /// Function intent contract:
    ///
    /// - Input: a decoded durable interaction continuation capsule.
    /// - Output: `Ok(())` only for a structurally valid `Restartable`
    ///   checkpoint whose intent exactly matches its request and fence.
    /// - Errors: returns a typed fail-closed reason for unknown versions,
    ///   missing or mismatched intents, executing/unsafe/unsupported
    ///   dispositions, or authority/fence mismatch.
    /// - State changes: none; validation neither mutates the capsule nor
    ///   performs recovery, persistence, presentation, or external effects.
    pub fn validate(&self) -> Result<(), DurableInteractionContinuationCapsuleError> {
        self.validate_structure()?;
        if self.disposition != DurableInteractionContinuationDisposition::Restartable {
            return Err(
                DurableInteractionContinuationCapsuleError::NonRestartableDisposition {
                    disposition: self.disposition,
                },
            );
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<(), DurableInteractionContinuationCapsuleError> {
        if self.version != DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION {
            return Err(
                DurableInteractionContinuationCapsuleError::UnsupportedVersion {
                    observed_version: self.version,
                },
            );
        }

        let authority = match &self.request {
            DurableInteractionContinuationRequest::ToolApproval { authority, .. }
            | DurableInteractionContinuationRequest::PermissionRequest { authority, .. } => {
                Some(authority)
            }
            DurableInteractionContinuationRequest::UserInput { .. }
            | DurableInteractionContinuationRequest::McpElicitation { .. } => None,
        };
        if authority.is_some_and(|authority| authority.operation_id() != &self.fence.operation_id) {
            return Err(
                DurableInteractionContinuationCapsuleError::AuthorityFenceMismatch {
                    kind: self.kind(),
                },
            );
        }

        match (
            self.disposition,
            self.execution_context_fingerprint.as_ref(),
        ) {
            (DurableInteractionContinuationDisposition::Restartable, None) => {
                return Err(
                    DurableInteractionContinuationCapsuleError::MissingExecutionContextFingerprint {
                        kind: self.kind(),
                    },
                );
            }
            (DurableInteractionContinuationDisposition::Restartable, Some(_)) | (_, None) => {}
            (_, Some(_)) => {
                return Err(
                    DurableInteractionContinuationCapsuleError::UnexpectedExecutionContextFingerprint {
                        disposition: self.disposition,
                    },
                );
            }
        }

        match (self.disposition, self.intent.as_ref()) {
            (DurableInteractionContinuationDisposition::Restartable, None) => {
                return Err(
                    DurableInteractionContinuationCapsuleError::MissingRestartableIntent {
                        kind: self.kind(),
                    },
                );
            }
            (DurableInteractionContinuationDisposition::Restartable, Some(intent)) => {
                self.validate_intent(intent)?;
            }
            (_, Some(_)) => {
                return Err(
                    DurableInteractionContinuationCapsuleError::UnexpectedIntentForDisposition {
                        disposition: self.disposition,
                    },
                );
            }
            (_, None) => {}
        }

        Ok(())
    }

    fn validate_intent(
        &self,
        intent: &DurableInteractionContinuationIntent,
    ) -> Result<(), DurableInteractionContinuationCapsuleError> {
        if self.kind() != intent.kind() {
            return Err(
                DurableInteractionContinuationCapsuleError::IntentKindMismatch {
                    request_kind: self.kind(),
                    intent_kind: intent.kind(),
                },
            );
        }

        match (&self.request, intent) {
            (
                DurableInteractionContinuationRequest::ToolApproval {
                    tool, authority, ..
                },
                DurableInteractionContinuationIntent::ToolInvocation(intent),
            ) => {
                if intent.invocation_id != tool.tool_call_id
                    || intent.invocation_id != intent.request.tool_call_id
                {
                    return Err(
                        DurableInteractionContinuationCapsuleError::InvocationIdMismatch {
                            kind: SurfaceInteractionKind::ToolApproval,
                        },
                    );
                }
                if intent.request != *tool || intent.authority != *authority {
                    return Err(
                        DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                            kind: SurfaceInteractionKind::ToolApproval,
                        },
                    );
                }
            }
            (
                DurableInteractionContinuationRequest::PermissionRequest {
                    tool_call_id,
                    permissions,
                    authority,
                    ..
                },
                DurableInteractionContinuationIntent::PermissionRetry(intent),
            ) => {
                if intent.invocation_id != *tool_call_id
                    || intent.invocation_id != intent.tool.tool_call_id
                {
                    return Err(
                        DurableInteractionContinuationCapsuleError::InvocationIdMismatch {
                            kind: SurfaceInteractionKind::PermissionRequest,
                        },
                    );
                }
                if intent.requested_permissions != *permissions || intent.authority != *authority {
                    return Err(
                        DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                            kind: SurfaceInteractionKind::PermissionRequest,
                        },
                    );
                }
            }
            (
                DurableInteractionContinuationRequest::UserInput { .. }
                | DurableInteractionContinuationRequest::McpElicitation { .. },
                DurableInteractionContinuationIntent::ContinuationTurn(intent),
            ) => {
                if intent.to_surface_request() != self.request.to_surface_request() {
                    return Err(
                        DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                            kind: self.kind(),
                        },
                    );
                }
                let expected_injection = match intent {
                    ContinuationTurnIntent::UserInput { .. } => {
                        ContinuationTurnAnswerInjection::user_input()
                    }
                    ContinuationTurnIntent::McpElicitation { .. } => {
                        ContinuationTurnAnswerInjection::mcp_elicitation()
                    }
                };
                if intent.injection() != &expected_injection {
                    return Err(
                        DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                            kind: self.kind(),
                        },
                    );
                }
            }
            _ => unreachable!("intent kind equality is checked before variant validation"),
        }

        Ok(())
    }

    pub const fn version(&self) -> u8 {
        self.version
    }

    pub fn interaction_id(&self) -> &SurfaceInteractionId {
        &self.interaction_id
    }

    pub fn fence(&self) -> &SurfaceOperationFence {
        &self.fence
    }

    pub fn kind(&self) -> SurfaceInteractionKind {
        self.request.kind()
    }

    pub fn request(&self) -> &DurableInteractionContinuationRequest {
        &self.request
    }

    pub fn execution_context_fingerprint(&self) -> Option<&Sha256Digest> {
        self.execution_context_fingerprint.as_ref()
    }

    pub const fn disposition(&self) -> DurableInteractionContinuationDisposition {
        self.disposition
    }

    pub fn intent(&self) -> Option<&DurableInteractionContinuationIntent> {
        self.intent.as_ref()
    }

    /// Validate that the recovered thread-owned runtime configuration and
    /// dependency snapshot matches the opaque fingerprint captured with this
    /// checkpoint. A mismatch is always fail closed.
    pub fn validate_for_execution_context(
        &self,
        observed_execution_context_fingerprint: &Sha256Digest,
    ) -> Result<(), DurableInteractionContinuationCapsuleError> {
        self.validate()?;
        if self.execution_context_fingerprint.as_ref()
            != Some(observed_execution_context_fingerprint)
        {
            return Err(
                DurableInteractionContinuationCapsuleError::ExecutionContextFingerprintMismatch,
            );
        }
        Ok(())
    }

    /// Return the restart intent only after applying all fail-closed version,
    /// disposition, identity, checkpoint, authority, and recovered execution
    /// context validation.
    pub fn restart_intent(
        &self,
        observed_execution_context_fingerprint: &Sha256Digest,
    ) -> Result<&DurableInteractionContinuationIntent, DurableInteractionContinuationCapsuleError>
    {
        self.validate_for_execution_context(observed_execution_context_fingerprint)?;
        self.intent.as_ref().ok_or(
            DurableInteractionContinuationCapsuleError::MissingRestartableIntent {
                kind: self.kind(),
            },
        )
    }
}

impl<'de> Deserialize<'de> for DurableInteractionContinuationCapsule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredCapsule {
            version: u8,
            interaction_id: SurfaceInteractionId,
            fence: SurfaceOperationFence,
            request: DurableInteractionContinuationRequest,
            execution_context_fingerprint: Option<Sha256Digest>,
            disposition: Option<DurableInteractionContinuationDisposition>,
            intent: Option<DurableInteractionContinuationIntent>,
        }

        let stored = StoredCapsule::deserialize(deserializer)?;
        let (version, execution_context_fingerprint, disposition, intent) = match stored.version {
            LEGACY_DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION => (
                DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION,
                None,
                DurableInteractionContinuationDisposition::Unsupported,
                None,
            ),
            DURABLE_INTERACTION_CONTINUATION_CAPSULE_VERSION => (
                stored.version,
                stored.execution_context_fingerprint,
                stored.disposition.ok_or_else(|| {
                    serde::de::Error::custom(
                        DurableInteractionContinuationCapsuleError::MissingDisposition,
                    )
                })?,
                stored.intent,
            ),
            observed_version => {
                return Err(serde::de::Error::custom(
                    DurableInteractionContinuationCapsuleError::UnsupportedVersion {
                        observed_version,
                    },
                ));
            }
        };
        let capsule = Self {
            version,
            interaction_id: stored.interaction_id,
            fence: stored.fence,
            request: stored.request,
            execution_context_fingerprint,
            disposition,
            intent,
        };
        capsule
            .validate_structure()
            .map_err(serde::de::Error::custom)?;
        Ok(capsule)
    }
}

impl DurableInteractionContinuationRequest {
    pub const fn kind(&self) -> SurfaceInteractionKind {
        match self {
            Self::ToolApproval { .. } => SurfaceInteractionKind::ToolApproval,
            Self::PermissionRequest { .. } => SurfaceInteractionKind::PermissionRequest,
            Self::UserInput { .. } => SurfaceInteractionKind::UserInput,
            Self::McpElicitation { .. } => SurfaceInteractionKind::McpElicitation,
        }
    }

    pub fn to_surface_request(&self) -> SurfaceInteractionRequest {
        match self {
            Self::ToolApproval {
                tool,
                description,
                preview,
                authority,
            } => SurfaceInteractionRequest::ToolApproval {
                tool: tool.clone(),
                description: description.clone(),
                preview: preview.clone(),
                authority: authority.clone(),
            },
            Self::PermissionRequest {
                tool_call_id,
                reason,
                permissions,
                authority,
            } => SurfaceInteractionRequest::PermissionRequest {
                tool_call_id: tool_call_id.clone(),
                reason: reason.clone(),
                permissions: permissions.clone(),
                authority: authority.clone(),
            },
            Self::UserInput {
                question,
                suggestions,
            } => SurfaceInteractionRequest::UserInput {
                question: question.clone(),
                suggestions: suggestions.clone(),
            },
            Self::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            } => SurfaceInteractionRequest::McpElicitation {
                server_name: server_name.clone(),
                server_request_id: server_request_id.clone(),
                message: message.clone(),
                request: request.clone(),
            },
        }
    }
}

impl TryFrom<SurfaceInteractionRequest> for DurableInteractionContinuationRequest {
    type Error = DurableInteractionContinuationCapsuleError;

    fn try_from(request: SurfaceInteractionRequest) -> Result<Self, Self::Error> {
        match request {
            SurfaceInteractionRequest::ToolApproval {
                tool,
                description,
                preview,
                authority,
            } => Ok(Self::ToolApproval {
                tool,
                description,
                preview,
                authority,
            }),
            SurfaceInteractionRequest::PermissionRequest {
                tool_call_id,
                reason,
                permissions,
                authority,
            } => Ok(Self::PermissionRequest {
                tool_call_id,
                reason,
                permissions,
                authority,
            }),
            SurfaceInteractionRequest::UserInput {
                question,
                suggestions,
            } => Ok(Self::UserInput {
                question,
                suggestions,
            }),
            SurfaceInteractionRequest::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            } => Ok(Self::McpElicitation {
                server_name,
                server_request_id,
                message,
                request,
            }),
            SurfaceInteractionRequest::BackgroundApproval { .. } => Err(
                DurableInteractionContinuationCapsuleError::UnsupportedInteractionKind {
                    kind: SurfaceInteractionKind::BackgroundApproval,
                },
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfacePermissionClientDecision {
    Allow {
        scope: PermissionGrantScope,
        permissions: SurfacePermissionProfile,
        strict_auto_review: bool,
    },
    Deny {
        scope: PermissionGrantScope,
        permissions: SurfacePermissionProfile,
        strict_auto_review: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceUserInputDecision {
    Answer(DisplayText),
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceMcpElicitationDecision {
    Accept { content: SurfaceDataValue },
    Decline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SurfaceClientInteractionAnswer {
    ToolApproval {
        decision: SurfaceAllowDeny,
    },
    PermissionRequest {
        decision: SurfacePermissionClientDecision,
    },
    UserInput {
        decision: SurfaceUserInputDecision,
    },
    McpElicitation {
        decision: SurfaceMcpElicitationDecision,
    },
    BackgroundApproval {
        decision: SurfaceAllowDeny,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerInteractionAnswerPolicy {
    NativeStrict,
    LegacyJsonlV0250PermissionProfile {
        connection_id: SurfaceConnectionId,
        policy_epoch: PolicyEpoch,
    },
    LegacyJsonlV0250McpOpaqueContent {
        connection_id: SurfaceConnectionId,
    },
}

#[derive(Clone, Eq, PartialEq)]
enum ApplicableAuthorityFingerprintKind {
    NotApplicable,
    Persisted { authority: AuthorityFingerprint },
}

#[derive(Clone, Eq, PartialEq)]
pub struct ApplicableAuthorityFingerprint(ApplicableAuthorityFingerprintKind);

#[allow(dead_code)]
impl ApplicableAuthorityFingerprint {
    pub(crate) const fn not_applicable() -> Self {
        Self(ApplicableAuthorityFingerprintKind::NotApplicable)
    }

    pub(crate) fn persisted(authority: AuthorityFingerprint) -> Self {
        Self(ApplicableAuthorityFingerprintKind::Persisted { authority })
    }

    pub fn authority(&self) -> Option<&AuthorityFingerprint> {
        match &self.0 {
            ApplicableAuthorityFingerprintKind::NotApplicable => None,
            ApplicableAuthorityFingerprintKind::Persisted { authority } => Some(authority),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct BoundInteractionResponse {
    response_id: SurfaceResponseId,
    answer: SurfaceClientInteractionAnswer,
    policy: BrokerInteractionAnswerPolicy,
    authority: ApplicableAuthorityFingerprint,
}

#[allow(dead_code)]
impl BoundInteractionResponse {
    pub(crate) fn new(
        response_id: SurfaceResponseId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
        authority: ApplicableAuthorityFingerprint,
    ) -> Self {
        Self {
            response_id,
            answer,
            policy,
            authority,
        }
    }

    pub fn response_id(&self) -> &SurfaceResponseId {
        &self.response_id
    }

    pub fn answer(&self) -> &SurfaceClientInteractionAnswer {
        &self.answer
    }

    pub fn policy(&self) -> &BrokerInteractionAnswerPolicy {
        &self.policy
    }

    pub fn authority(&self) -> &ApplicableAuthorityFingerprint {
        &self.authority
    }
}

#[derive(Clone, PartialEq)]
pub struct ValidatedInteractionResponse {
    interaction_id: SurfaceInteractionId,
    response_id: SurfaceResponseId,
    answer: SurfaceClientInteractionAnswer,
    policy: BrokerInteractionAnswerPolicy,
    authority: ApplicableAuthorityFingerprint,
    route_epoch: ResponseRouteEpoch,
    operation_fence: SurfaceOperationFence,
}

#[allow(dead_code)]
impl ValidatedInteractionResponse {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        interaction_id: SurfaceInteractionId,
        response_id: SurfaceResponseId,
        answer: SurfaceClientInteractionAnswer,
        policy: BrokerInteractionAnswerPolicy,
        authority: ApplicableAuthorityFingerprint,
        route_epoch: ResponseRouteEpoch,
        operation_fence: SurfaceOperationFence,
    ) -> Self {
        Self {
            interaction_id,
            response_id,
            answer,
            policy,
            authority,
            route_epoch,
            operation_fence,
        }
    }

    pub fn interaction_id(&self) -> &SurfaceInteractionId {
        &self.interaction_id
    }

    pub fn response_id(&self) -> &SurfaceResponseId {
        &self.response_id
    }

    pub fn answer(&self) -> &SurfaceClientInteractionAnswer {
        &self.answer
    }

    pub fn policy(&self) -> &BrokerInteractionAnswerPolicy {
        &self.policy
    }

    pub fn authority(&self) -> &ApplicableAuthorityFingerprint {
        &self.authority
    }

    pub const fn route_epoch(&self) -> ResponseRouteEpoch {
        self.route_epoch
    }

    pub fn operation_fence(&self) -> &SurfaceOperationFence {
        &self.operation_fence
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SurfaceInteractionSafeProjection {
    ToolApproval {
        allowed: bool,
    },
    PermissionRequest {
        decision: SurfaceAllowDeny,
        scope: PermissionGrantScope,
        strict_auto_review: bool,
    },
    UserInput {
        answered: bool,
    },
    McpElicitation {
        accepted: bool,
    },
    BackgroundApproval {
        allowed: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInteractionResolutionReceipt {
    pub response_id: SurfaceResponseId,
    pub receipt_id: SurfaceResponseReceiptId,
    pub kind: SurfaceInteractionKind,
    pub safe_projection: SurfaceInteractionSafeProjection,
}

/// Stable durable identity for the ordinary operation that continues one
/// resolved UserInput/MCP interaction. Every component is deterministically
/// derived from the interaction identity and its first-valid resolution
/// receipt, so recovery can only converge on the same operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableInteractionContinuationOperationIdentity {
    dispatch_id: SurfaceSettlementId,
    operation_id: SurfaceOperationId,
    request_id: SurfaceRequestId,
    turn_id: SurfaceTurnId,
}

impl DurableInteractionContinuationOperationIdentity {
    /// Function intent contract: derive the one durable continuation
    /// operation identity for a resolved UserInput/MCP interaction without
    /// allocating process-local identity or performing I/O.
    pub(crate) fn try_new(
        interaction_id: &SurfaceInteractionId,
        receipt: &SurfaceInteractionResolutionReceipt,
    ) -> Result<Self, DurableInteractionContinuationCapsuleError> {
        if !matches!(
            receipt.kind,
            SurfaceInteractionKind::UserInput | SurfaceInteractionKind::McpElicitation
        ) {
            return Err(
                DurableInteractionContinuationCapsuleError::UnsupportedInteractionKind {
                    kind: receipt.kind,
                },
            );
        }
        Ok(Self {
            dispatch_id: SurfaceSettlementId::try_from_bytes(*interaction_id.as_bytes())
                .expect("interaction identity is a UUIDv7"),
            operation_id: SurfaceOperationId::try_from_bytes(*receipt.receipt_id.as_bytes())
                .expect("resolution receipt identity is a UUIDv7"),
            request_id: SurfaceRequestId::try_from_bytes(*receipt.response_id.as_bytes())
                .expect("response identity is a UUIDv7"),
            turn_id: SurfaceTurnId::parse(format!(
                "turn_{}",
                uuid::Uuid::from_bytes(*receipt.response_id.as_bytes())
            ))
            .expect("response identity produces a valid turn identity"),
        })
    }

    pub(crate) fn dispatch_id(&self) -> &SurfaceSettlementId {
        &self.dispatch_id
    }

    pub(crate) fn operation_id(&self) -> &SurfaceOperationId {
        &self.operation_id
    }

    pub(crate) fn request_id(&self) -> &SurfaceRequestId {
        &self.request_id
    }

    pub(crate) fn turn_id(&self) -> &SurfaceTurnId {
        &self.turn_id
    }
}

const DURABLE_INTERACTION_CONTINUATION_ANSWER_VERSION: u8 = 1;

/// Private durable payload for a cold-recovered continuation answer. The
/// record is journaled with the public safe resolution receipt, but its fields
/// are intentionally not exposed through the public surface projection API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DurableInteractionContinuationAnswer {
    version: u8,
    interaction_id: SurfaceInteractionId,
    fence: SurfaceOperationFence,
    response_id: SurfaceResponseId,
    receipt_id: SurfaceResponseReceiptId,
    request_identity: NonEmptyText,
    injection: ContinuationTurnAnswerInjection,
    payload: DurableInteractionContinuationAnswerPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum DurableInteractionContinuationAnswerPayload {
    UserInput { answer: DisplayText },
    McpElicitation { content: SurfaceDataValue },
}

impl DurableInteractionContinuationAnswer {
    /// Freeze the accepted private answer against the already-created capsule
    /// and the public resolution receipt. Cancel/decline returns `None` and is
    /// therefore never serialized as a private answer fact.
    pub(crate) fn try_new(
        capsule: &DurableInteractionContinuationCapsule,
        receipt: &SurfaceInteractionResolutionReceipt,
        answer: &SurfaceClientInteractionAnswer,
    ) -> Result<Option<Self>, DurableInteractionContinuationCapsuleError> {
        capsule.validate()?;
        let intent = match capsule.intent() {
            Some(DurableInteractionContinuationIntent::ContinuationTurn(intent)) => intent,
            _ => {
                return Err(
                    DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                        kind: capsule.kind(),
                    },
                );
            }
        };
        if receipt.kind != capsule.kind() {
            return Err(
                DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                    kind: capsule.kind(),
                },
            );
        }
        let payload = match (intent, answer, &receipt.safe_projection) {
            (
                ContinuationTurnIntent::UserInput { .. },
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(answer),
                },
                SurfaceInteractionSafeProjection::UserInput { answered: true },
            ) => DurableInteractionContinuationAnswerPayload::UserInput {
                answer: answer.clone(),
            },
            (
                ContinuationTurnIntent::McpElicitation { .. },
                SurfaceClientInteractionAnswer::McpElicitation {
                    decision: SurfaceMcpElicitationDecision::Accept { content },
                },
                SurfaceInteractionSafeProjection::McpElicitation { accepted: true },
            ) => DurableInteractionContinuationAnswerPayload::McpElicitation {
                content: content.clone(),
            },
            (
                ContinuationTurnIntent::UserInput { .. },
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Cancel,
                },
                SurfaceInteractionSafeProjection::UserInput { answered: false },
            )
            | (
                ContinuationTurnIntent::McpElicitation { .. },
                SurfaceClientInteractionAnswer::McpElicitation {
                    decision: SurfaceMcpElicitationDecision::Decline,
                },
                SurfaceInteractionSafeProjection::McpElicitation { accepted: false },
            ) => return Ok(None),
            _ => {
                return Err(
                    DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                        kind: capsule.kind(),
                    },
                );
            }
        };
        Ok(Some(Self {
            version: DURABLE_INTERACTION_CONTINUATION_ANSWER_VERSION,
            interaction_id: capsule.interaction_id().clone(),
            fence: capsule.fence().clone(),
            response_id: receipt.response_id.clone(),
            receipt_id: receipt.receipt_id.clone(),
            request_identity: intent.request_identity().clone(),
            injection: intent.injection().clone(),
            payload,
        }))
    }

    pub(crate) fn validate(
        &self,
        capsule: &DurableInteractionContinuationCapsule,
        receipt: &SurfaceInteractionResolutionReceipt,
    ) -> Result<(), DurableInteractionContinuationCapsuleError> {
        capsule.validate()?;
        let intent = match capsule.intent() {
            Some(DurableInteractionContinuationIntent::ContinuationTurn(intent)) => intent,
            _ => {
                return Err(
                    DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                        kind: capsule.kind(),
                    },
                );
            }
        };
        let payload_matches = matches!(
            (intent, &self.payload, &receipt.safe_projection),
            (
                ContinuationTurnIntent::UserInput { .. },
                DurableInteractionContinuationAnswerPayload::UserInput { .. },
                SurfaceInteractionSafeProjection::UserInput { answered: true },
            ) | (
                ContinuationTurnIntent::McpElicitation { .. },
                DurableInteractionContinuationAnswerPayload::McpElicitation { .. },
                SurfaceInteractionSafeProjection::McpElicitation { accepted: true },
            )
        );
        if self.version != DURABLE_INTERACTION_CONTINUATION_ANSWER_VERSION
            || self.interaction_id != *capsule.interaction_id()
            || self.fence != *capsule.fence()
            || self.response_id != receipt.response_id
            || self.receipt_id != receipt.receipt_id
            || self.request_identity != *intent.request_identity()
            || self.injection != *intent.injection()
            || receipt.kind != capsule.kind()
            || !payload_matches
        {
            return Err(
                DurableInteractionContinuationCapsuleError::RequestIntentMismatch {
                    kind: capsule.kind(),
                },
            );
        }
        Ok(())
    }

    pub(crate) fn interaction_id(&self) -> &SurfaceInteractionId {
        &self.interaction_id
    }

    pub(crate) fn fence(&self) -> &SurfaceOperationFence {
        &self.fence
    }

    pub(crate) fn receipt_id(&self) -> &SurfaceResponseReceiptId {
        &self.receipt_id
    }

    pub(crate) fn response_id(&self) -> &SurfaceResponseId {
        &self.response_id
    }

    pub(crate) fn request_identity(&self) -> &NonEmptyText {
        &self.request_identity
    }

    pub(crate) fn injection(&self) -> &ContinuationTurnAnswerInjection {
        &self.injection
    }

    pub(crate) fn answer_text(&self) -> String {
        match &self.payload {
            DurableInteractionContinuationAnswerPayload::UserInput { answer } => {
                answer.as_str().to_string()
            }
            DurableInteractionContinuationAnswerPayload::McpElicitation { content } => {
                serde_json::to_string(content).expect("surface data value is serializable")
            }
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct BrokerInteractionRequestRecord {
    pub thread_id: SurfaceThreadId,
    pub interaction_id: SurfaceInteractionId,
    pub fence: SurfaceOperationFence,
    pub kind: SurfaceInteractionKind,
    pub request: SurfaceInteractionRequest,
    pub response_token: SurfaceResponseToken,
    pub answer_policy: BrokerInteractionAnswerPolicy,
    pub recovery_disposition: InteractionUnavailableDisposition,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerResponsePayload {
    ReplayablePrivate { encrypted_reference: OpaqueToken },
    LiveOnly { incarnation: SurfaceIncarnation },
}

#[derive(Clone, Eq, PartialEq)]
pub struct BrokerInteractionResponseRecord {
    pub receipt: SurfaceInteractionResolutionReceipt,
    pub payload: BrokerResponsePayload,
    pub keyed_response_digest: OpaqueToken,
}

#[derive(Clone, Eq, PartialEq)]
pub enum BrokerInteractionWaitResult {
    Resolved {
        response: BrokerInteractionResponseRecord,
    },
    Cancelled {
        reason: InteractionCancelReason,
    },
    Expired {
        deadline: InteractionExpiryDeadline,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InteractionCancelReason {
    OperationCancelled {
        reason: CancelReason,
    },
    HostShutdown,
    ThreadClose,
    CapabilityUnavailable,
    ExpiryAuthorityUnavailable {
        deadline: InteractionExpiryDeadline,
        failure: InteractionExpiryAuthorityFailure,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceInteractionLifecycle {
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
        background_fence: SurfaceBackgroundFence,
    },
}

#[derive(Clone, PartialEq)]
pub struct SurfaceInteractionView {
    pub interaction_id: SurfaceInteractionId,
    pub revision: InteractionRevision,
    pub fence: SurfaceOperationFence,
    pub kind: SurfaceInteractionKind,
    pub request: SurfaceInteractionRequest,
    pub route: SurfaceInteractionRoute,
    pub lifecycle: SurfaceInteractionLifecycle,
    pub recovery_disposition: InteractionUnavailableDisposition,
}

#[derive(Clone, PartialEq)]
pub enum InteractionPatch {
    Requested {
        interaction: SurfaceInteractionView,
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
        continuation: Option<DurableInteractionContinuationAnswer>,
    },
    ContinuationDispatchStarted {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: SurfaceResponseReceiptId,
        dispatch_id: SurfaceSettlementId,
        operation_id: SurfaceOperationId,
        turn_id: SurfaceTurnId,
    },
    ContinuationDispatchConsumed {
        interaction_id: SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: SurfaceResponseReceiptId,
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
        background_fence: SurfaceBackgroundFence,
        route: SurfaceInteractionRoute,
    },
}

#[derive(Serialize)]
enum CanonicalInteractionRequestV1<'a> {
    ToolApproval {
        tool: &'a SurfaceToolRequest,
        description: &'a DisplayText,
        preview: &'a Option<DisplayText>,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
    PermissionRequest {
        tool_call_id: &'a SurfaceToolCallId,
        reason: &'a Option<DisplayText>,
        permissions: &'a SurfacePermissionProfile,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
    UserInput {
        question: &'a NonEmptyText,
        suggestions: &'a Vec<DisplayText>,
    },
    McpElicitation {
        server_name: &'a NonEmptyText,
        server_request_id: &'a NonEmptyText,
        message: &'a DisplayText,
        request: &'a SurfaceMcpElicitationRequest,
    },
    BackgroundApproval {
        task: CanonicalTaskFenceV1<'a>,
        tool: &'a SurfaceToolRequest,
        authority: CanonicalAuthorityFingerprintV1<'a>,
    },
}

fn canonical_interaction_request_v1(
    request: &SurfaceInteractionRequest,
) -> CanonicalInteractionRequestV1<'_> {
    match request {
        SurfaceInteractionRequest::ToolApproval {
            tool,
            description,
            preview,
            authority,
        } => CanonicalInteractionRequestV1::ToolApproval {
            tool,
            description,
            preview,
            authority: canonical_authority_fingerprint_v1(authority),
        },
        SurfaceInteractionRequest::PermissionRequest {
            tool_call_id,
            reason,
            permissions,
            authority,
        } => CanonicalInteractionRequestV1::PermissionRequest {
            tool_call_id,
            reason,
            permissions,
            authority: canonical_authority_fingerprint_v1(authority),
        },
        SurfaceInteractionRequest::UserInput {
            question,
            suggestions,
        } => CanonicalInteractionRequestV1::UserInput {
            question,
            suggestions,
        },
        SurfaceInteractionRequest::McpElicitation {
            server_name,
            server_request_id,
            message,
            request,
        } => CanonicalInteractionRequestV1::McpElicitation {
            server_name,
            server_request_id,
            message,
            request,
        },
        SurfaceInteractionRequest::BackgroundApproval {
            task,
            tool,
            authority,
        } => CanonicalInteractionRequestV1::BackgroundApproval {
            task: canonical_task_fence_v1(task),
            tool,
            authority: canonical_authority_fingerprint_v1(authority),
        },
    }
}

#[derive(Serialize)]
enum CanonicalInteractionLifecycleV1<'a> {
    Requested,
    Resolved {
        receipt: &'a SurfaceInteractionResolutionReceipt,
    },
    Cancelled {
        reason: &'a InteractionCancelReason,
    },
    Expired {
        deadline: &'a InteractionExpiryDeadline,
    },
    Transferred {
        background_fence: CanonicalBackgroundFenceV1<'a>,
    },
}

fn canonical_interaction_lifecycle_v1(
    lifecycle: &SurfaceInteractionLifecycle,
) -> CanonicalInteractionLifecycleV1<'_> {
    match lifecycle {
        SurfaceInteractionLifecycle::Requested => CanonicalInteractionLifecycleV1::Requested,
        SurfaceInteractionLifecycle::Resolved { receipt } => {
            CanonicalInteractionLifecycleV1::Resolved { receipt }
        }
        SurfaceInteractionLifecycle::Cancelled { reason } => {
            CanonicalInteractionLifecycleV1::Cancelled { reason }
        }
        SurfaceInteractionLifecycle::Expired { deadline } => {
            CanonicalInteractionLifecycleV1::Expired { deadline }
        }
        SurfaceInteractionLifecycle::Transferred { background_fence } => {
            CanonicalInteractionLifecycleV1::Transferred {
                background_fence: canonical_background_fence_v1(background_fence),
            }
        }
    }
}

#[derive(Serialize)]
pub(super) struct CanonicalInteractionViewV1<'a> {
    interaction_id: &'a SurfaceInteractionId,
    revision: InteractionRevision,
    fence: &'a SurfaceOperationFence,
    kind: SurfaceInteractionKind,
    request: CanonicalInteractionRequestV1<'a>,
    route: &'a SurfaceInteractionRoute,
    lifecycle: CanonicalInteractionLifecycleV1<'a>,
    recovery_disposition: &'a InteractionUnavailableDisposition,
}

fn canonical_interaction_view_v1(
    interaction: &SurfaceInteractionView,
) -> CanonicalInteractionViewV1<'_> {
    CanonicalInteractionViewV1 {
        interaction_id: &interaction.interaction_id,
        revision: interaction.revision,
        fence: &interaction.fence,
        kind: interaction.kind,
        request: canonical_interaction_request_v1(&interaction.request),
        route: &interaction.route,
        lifecycle: canonical_interaction_lifecycle_v1(&interaction.lifecycle),
        recovery_disposition: &interaction.recovery_disposition,
    }
}

#[derive(Serialize)]
pub(super) enum CanonicalInteractionPatchV1<'a> {
    Requested {
        interaction: CanonicalInteractionViewV1<'a>,
    },
    RouteChanged {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        route: &'a SurfaceInteractionRoute,
    },
    Resolved {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt: &'a SurfaceInteractionResolutionReceipt,
        continuation: &'a Option<DurableInteractionContinuationAnswer>,
    },
    ContinuationDispatchStarted {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: &'a SurfaceResponseReceiptId,
        dispatch_id: &'a SurfaceSettlementId,
        operation_id: &'a SurfaceOperationId,
        turn_id: &'a SurfaceTurnId,
    },
    ContinuationDispatchConsumed {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        receipt_id: &'a SurfaceResponseReceiptId,
        dispatch_id: &'a SurfaceSettlementId,
        operation_id: &'a SurfaceOperationId,
        turn_id: &'a SurfaceTurnId,
    },
    Cancelled {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        reason: &'a InteractionCancelReason,
    },
    Expired {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        deadline: &'a InteractionExpiryDeadline,
    },
    Transferred {
        interaction_id: &'a SurfaceInteractionId,
        expected_revision: InteractionRevision,
        next_revision: InteractionRevision,
        background_fence: CanonicalBackgroundFenceV1<'a>,
        route: &'a SurfaceInteractionRoute,
    },
}

pub(super) fn canonical_interaction_patch_v1(
    patch: &InteractionPatch,
) -> CanonicalInteractionPatchV1<'_> {
    match patch {
        InteractionPatch::Requested { interaction } => CanonicalInteractionPatchV1::Requested {
            interaction: canonical_interaction_view_v1(interaction),
        },
        InteractionPatch::RouteChanged {
            interaction_id,
            expected_revision,
            next_revision,
            route,
        } => CanonicalInteractionPatchV1::RouteChanged {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            route,
        },
        InteractionPatch::Resolved {
            interaction_id,
            expected_revision,
            next_revision,
            receipt,
            continuation,
        } => CanonicalInteractionPatchV1::Resolved {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            receipt,
            continuation,
        },
        InteractionPatch::ContinuationDispatchStarted {
            interaction_id,
            expected_revision,
            next_revision,
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
        } => CanonicalInteractionPatchV1::ContinuationDispatchStarted {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
        },
        InteractionPatch::ContinuationDispatchConsumed {
            interaction_id,
            expected_revision,
            next_revision,
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
        } => CanonicalInteractionPatchV1::ContinuationDispatchConsumed {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            receipt_id,
            dispatch_id,
            operation_id,
            turn_id,
        },
        InteractionPatch::Cancelled {
            interaction_id,
            expected_revision,
            next_revision,
            reason,
        } => CanonicalInteractionPatchV1::Cancelled {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            reason,
        },
        InteractionPatch::Expired {
            interaction_id,
            expected_revision,
            next_revision,
            deadline,
        } => CanonicalInteractionPatchV1::Expired {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            deadline,
        },
        InteractionPatch::Transferred {
            interaction_id,
            expected_revision,
            next_revision,
            background_fence,
            route,
        } => CanonicalInteractionPatchV1::Transferred {
            interaction_id,
            expected_revision: *expected_revision,
            next_revision: *next_revision,
            background_fence: canonical_background_fence_v1(background_fence),
            route,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_surface::identity::{
        SurfaceBackgroundOwnerToken, SurfaceGenerationId, ThreadOwnerEpoch,
    };

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    fn continuation_fence(seed: u8) -> SurfaceOperationFence {
        SurfaceOperationFence {
            thread_id: SurfaceThreadId::try_from_bytes([seed; 16]).unwrap(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            generation_id: SurfaceGenerationId::new(0),
        }
    }

    fn continuation_receipt(
        seed: u8,
        kind: SurfaceInteractionKind,
        safe_projection: SurfaceInteractionSafeProjection,
    ) -> SurfaceInteractionResolutionReceipt {
        SurfaceInteractionResolutionReceipt {
            response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            receipt_id: SurfaceResponseReceiptId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap(),
            kind,
            safe_projection,
        }
    }

    #[test]
    fn continuation_operation_identity_is_stable_and_resolution_bound() {
        let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(10)).unwrap();
        let first_receipt = continuation_receipt(
            11,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionSafeProjection::UserInput { answered: true },
        );
        let second_receipt = continuation_receipt(
            21,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionSafeProjection::UserInput { answered: true },
        );

        let first = DurableInteractionContinuationOperationIdentity::try_new(
            &interaction_id,
            &first_receipt,
        )
        .unwrap();
        let repeated = DurableInteractionContinuationOperationIdentity::try_new(
            &interaction_id,
            &first_receipt,
        )
        .unwrap();
        let second = DurableInteractionContinuationOperationIdentity::try_new(
            &interaction_id,
            &second_receipt,
        )
        .unwrap();

        assert_eq!(first, repeated);
        assert_eq!(first.dispatch_id(), second.dispatch_id());
        assert_ne!(first.operation_id(), second.operation_id());
        assert_ne!(first.request_id(), second.request_id());
        assert_ne!(first.turn_id(), second.turn_id());
        assert_eq!(
            first.operation_id().as_bytes(),
            first_receipt.receipt_id.as_bytes()
        );
    }

    #[test]
    fn continuation_cancel_and_decline_never_create_private_answer_facts() {
        let user_interaction = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(30)).unwrap();
        let user_request = SurfaceInteractionRequest::UserInput {
            question: NonEmptyText::try_new("continue?").unwrap(),
            suggestions: Vec::new(),
        };
        let user_capsule = DurableInteractionContinuationCapsule::try_new(
            user_interaction,
            continuation_fence(31),
            user_request,
            Sha256Digest::new([31; 32]),
        )
        .unwrap();
        let user_receipt = continuation_receipt(
            32,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionSafeProjection::UserInput { answered: false },
        );
        assert_eq!(
            DurableInteractionContinuationAnswer::try_new(
                &user_capsule,
                &user_receipt,
                &SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Cancel,
                },
            )
            .unwrap(),
            None
        );

        let mcp_interaction = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(40)).unwrap();
        let mcp_request = SurfaceInteractionRequest::McpElicitation {
            server_name: NonEmptyText::try_new("server").unwrap(),
            server_request_id: NonEmptyText::try_new("request").unwrap(),
            message: DisplayText::new("provide input"),
            request: SurfaceMcpElicitationRequest::Form {
                requested_schema: None,
                supported_schema: None,
            },
        };
        let mcp_capsule = DurableInteractionContinuationCapsule::try_new(
            mcp_interaction,
            continuation_fence(41),
            mcp_request,
            Sha256Digest::new([41; 32]),
        )
        .unwrap();
        let mcp_receipt = continuation_receipt(
            42,
            SurfaceInteractionKind::McpElicitation,
            SurfaceInteractionSafeProjection::McpElicitation { accepted: false },
        );
        assert_eq!(
            DurableInteractionContinuationAnswer::try_new(
                &mcp_capsule,
                &mcp_receipt,
                &SurfaceClientInteractionAnswer::McpElicitation {
                    decision: SurfaceMcpElicitationDecision::Decline,
                },
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn broker_only_route_and_transferred_patch_remain_constructible_in_runtime() {
        let operation_fence = SurfaceOperationFence {
            thread_id: SurfaceThreadId::try_from_bytes([1; 16]).unwrap(),
            thread_owner_epoch: ThreadOwnerEpoch::new(0),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            generation_id: SurfaceGenerationId::new(0),
        };
        let background_fence = SurfaceBackgroundFence {
            operation_fence,
            background_owner_token: SurfaceBackgroundOwnerToken::new([1; 32]),
        };
        let _route = BrokerInteractionResponseRoute::Exclusive {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
            attachment_id: SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(2)).unwrap(),
            grant_token: SurfaceResponseGrantToken::new([2; 32]),
        };
        let _patch = InteractionPatch::Transferred {
            interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(3)).unwrap(),
            expected_revision: InteractionRevision::try_new(1).unwrap(),
            next_revision: InteractionRevision::try_new(2).unwrap(),
            background_fence,
            route: SurfaceInteractionRoute::Unassigned {
                epoch: ResponseRouteEpoch::try_new(1).unwrap(),
            },
        };
    }
}
