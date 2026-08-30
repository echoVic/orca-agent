//! TUI protocol values crossing the renderer, runtime, and action boundaries.
//!
//! These values are deliberately kept separate from [`crate::types::AppState`].
//! Producers and consumers import the protocol owner directly.

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::OperationId;
use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::PlanItem;
use orca_runtime::mentions::MentionBindings;
use orca_runtime::runtime_permission::RuntimePermissionRequestKind;
use orca_runtime::surface::{
    RuntimeSurfaceThreadHandle, SurfaceOperationId, SurfaceReadError, SurfaceReadErrorCode,
    SurfaceReadResult, SurfaceReadRevision, TaskTranscriptSnapshot,
};

use crate::clipboard_image::ImagePasteRequest;
use crate::composer_images::ComposerImageAttachment;
use crate::transcript_state::ChatMessage;
use crate::types::SideParentStatus;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionAttachmentId(u64);

impl SessionAttachmentId {
    pub(crate) const fn new(value: u64) -> Self {
        assert!(value != 0, "session attachment ids start at one");
        Self(value)
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub enum TuiMemoryScope {
    User,
    Project,
}

#[doc(hidden)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoalDraft {
    pub objective: String,
    pub pending_pastes: Vec<(String, String)>,
}

impl From<String> for GoalDraft {
    fn from(objective: String) -> Self {
        Self {
            objective,
            pending_pastes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWorkflowNotification {
    pub id: String,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TuiInteractionKind {
    Approval,
    Permission,
    UserInput,
    McpElicitation,
}

/// Input mode requested by an MCP elicitation event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiMcpElicitationMode {
    Form,
    Url,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TuiInteractionKey {
    pub operation_id: OperationId,
    pub request_id: String,
    pub kind: TuiInteractionKind,
}

impl TuiInteractionKey {
    pub fn new(
        operation_id: OperationId,
        request_id: impl Into<String>,
        kind: TuiInteractionKind,
    ) -> Self {
        Self {
            operation_id,
            request_id: request_id.into(),
            kind,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiInteractionResponse {
    Approval(bool),
    Permission(bool),
    UserInput(String),
    McpElicitation {
        accepted: bool,
        content_json: Option<String>,
    },
}

impl TuiInteractionResponse {
    pub fn kind(&self) -> TuiInteractionKind {
        match self {
            Self::Approval(_) => TuiInteractionKind::Approval,
            Self::Permission(_) => TuiInteractionKind::Permission,
            Self::UserInput(_) => TuiInteractionKind::UserInput,
            Self::McpElicitation { .. } => TuiInteractionKind::McpElicitation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PendingTuiInput {
    UserInput(TuiInteractionKey),
    McpElicitation(TuiInteractionKey),
}

impl PendingTuiInput {
    pub fn key(&self) -> &TuiInteractionKey {
        match self {
            Self::UserInput(key) | Self::McpElicitation(key) => key,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttachedTuiEvent {
    pub(crate) attachment: Option<SessionAttachmentId>,
    pub(crate) event: TuiEvent,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TuiEvent {
    #[doc(hidden)]
    Attached(Box<AttachedTuiEvent>),
    #[doc(hidden)]
    SessionAttachmentActivated,
    SideConversationChanged {
        active: bool,
        available: bool,
        parent_thread_id: String,
        parent_title: String,
        parent_status: SideParentStatus,
    },
    SideParentStatusChanged(SideParentStatus),
    #[doc(hidden)]
    SurfaceProjectionSynced(Box<crate::surface_projection::SurfaceProjectionState>),
    TurnStarted {
        turn: u32,
        task: Option<TuiTaskLifecycle>,
    },
    QueuedSubmissionStarted {
        id: u64,
    },
    PromptQueueUpdated(orca_runtime::prompt_queue::PromptQueueSnapshot),
    PromptQueueControlUpdated {
        deleted_id: Option<orca_runtime::prompt_queue::QueuedSubmissionId>,
        snapshot: orca_runtime::prompt_queue::PromptQueueSnapshot,
    },
    ReasoningDelta(String),
    MessageDelta(String),
    AssistantAttemptDiscarded,
    AssistantResponseCompleted(Option<String>, Option<String>),
    ToolRequested {
        id: String,
        name: String,
        target: Option<String>,
    },
    ToolCallProgress {
        id: String,
        name: Option<String>,
        arguments_bytes: usize,
    },
    ToolOutputDelta {
        id: String,
        chunk: String,
    },
    ToolCompleted {
        id: String,
        name: String,
        status: String,
        output: String,
        diff: Option<String>,
        kind: Option<String>,
    },
    PlanUpdated {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    SubagentStarted {
        id: String,
        description: String,
    },
    SubagentCompleted {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
    },
    SubagentProgress {
        id: String,
        activity: String,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    },
    BackgroundTaskOutputAttached {
        task_id: String,
    },
    /// Result of a checkpoint-backed child transcript read. The payload is
    /// deliberately typed so the renderer never needs to inspect runtime
    /// stores or continuation paths.
    TaskTranscriptResult {
        request: TaskTranscriptRequest,
        result: TaskTranscriptResult,
    },
    WorkflowNotification {
        id: String,
        prompt: String,
        status: String,
        summary: String,
    },
    ApprovalNeeded {
        key: TuiInteractionKey,
        tool: String,
        target: Option<String>,
        preview: Option<String>,
    },
    PermissionApprovalNeeded {
        key: TuiInteractionKey,
        tool: String,
        target: Option<String>,
        preview: Option<String>,
        permission_kind: RuntimePermissionRequestKind,
    },
    UserInputRequested {
        key: TuiInteractionKey,
        question: String,
        choices: Vec<String>,
    },
    McpElicitationRequested {
        key: TuiInteractionKey,
        server_name: String,
        mode: TuiMcpElicitationMode,
        message: String,
        url: Option<String>,
        requested_schema_json: Option<String>,
    },
    HistoryLoaded {
        messages: Vec<ChatMessage>,
        plan: Option<(Option<String>, Vec<PlanItem>)>,
        label: String,
    },
    NewSessionStarted,
    SessionProjectionReset(Box<crate::surface_projection::SurfaceProjectionState>),
    SavedSessionsUpdated {
        sessions: Vec<orca_runtime::history::SessionSummary>,
        next_offset: Option<usize>,
        backfill_complete: bool,
        notice: String,
    },
    SavedSessionActionFailed(String),
    Notice(String),
    MentionSearchDirty {
        generation: orca_file_search::SessionGeneration,
    },
    MentionCatalogDirty {
        generation: u64,
    },
    MentionRuntimeReady(RuntimeSurfaceThreadHandle),
    ClipboardImagePasteCompleted {
        request_id: u64,
        result: Result<Vec<crate::clipboard_image::ClipboardImagePayload>, String>,
    },
    SubmissionRejected {
        queued_id: Option<u64>,
        prompt: String,
        bindings: MentionBindings,
        images: Vec<ComposerImageAttachment>,
        message: String,
    },
    OperationRejected(String),
    Error(String),
    CompactionStarted,
    SessionCompleted {
        status: String,
    },
    Compacted {
        before_messages: usize,
        after_messages: usize,
        reason: String,
        strategy: String,
        collapsed_messages: usize,
        status_text: String,
    },
    SettingsUpdated {
        model: String,
        reasoning_effort: orca_core::config::ReasoningEffort,
        approval_mode: ApprovalMode,
    },
    PlanImplementationStarted {
        prompt: String,
    },
    GoalStatus(Option<orca_core::goal_types::ThreadGoal>),
    Backtracked {
        prompt: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiTaskLifecycle {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub turn: u32,
}

/// A read-only child transcript lookup. The runtime validates the task's
/// current surface publication revision before returning any transcript content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTranscriptRequest {
    pub task_id: String,
    pub expected_revision: u64,
}

/// A safe, UI-owned representation of a runtime transcript read error.
/// `current_revision` is present only for stale task fences; no opaque runtime
/// token or filesystem path crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTranscriptError {
    pub code: SurfaceReadErrorCode,
    pub message: String,
    pub current_revision: Option<u64>,
}

/// Typed result consumed by the TUI transcript detail state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskTranscriptResult {
    Found(TaskTranscriptSnapshot),
    NotFound(TaskTranscriptError),
    Invalid(TaskTranscriptError),
    Stale(TaskTranscriptError),
    Unavailable(TaskTranscriptError),
}

impl TaskTranscriptResult {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(TaskTranscriptError {
            code: SurfaceReadErrorCode::InvalidRequest,
            message: message.into(),
            current_revision: None,
        })
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(TaskTranscriptError {
            code: SurfaceReadErrorCode::RuntimeUnavailable,
            message: message.into(),
            current_revision: None,
        })
    }

    pub(crate) fn from_surface(result: SurfaceReadResult<TaskTranscriptSnapshot>) -> Self {
        match result {
            SurfaceReadResult::Found { value, .. } => Self::Found(value),
            SurfaceReadResult::NotFound { error, .. } => Self::NotFound(error.into()),
            SurfaceReadResult::Invalid { error, .. } => Self::Invalid(error.into()),
            SurfaceReadResult::Stale { error, .. } => Self::Stale(error.into()),
            SurfaceReadResult::Unavailable { error, .. } => Self::Unavailable(error.into()),
        }
    }
}

impl From<SurfaceReadError> for TaskTranscriptError {
    fn from(error: SurfaceReadError) -> Self {
        let current_revision = match error.current_revision {
            Some(SurfaceReadRevision::Task { revision, .. }) => Some(revision.get()),
            _ => None,
        };
        // Runtime diagnostics can contain provider-controlled text. Keep the
        // TUI contract on stable, path-free messages rather than forwarding a
        // raw error string across the surface boundary.
        let message = match error.code {
            SurfaceReadErrorCode::InvalidRequest => "invalid task transcript request",
            SurfaceReadErrorCode::NotFound => "task transcript was not found",
            SurfaceReadErrorCode::StaleRevision => "task transcript revision is stale",
            SurfaceReadErrorCode::BindingMismatch => "task transcript binding is invalid",
            SurfaceReadErrorCode::CapabilityDenied => "task transcript access was denied",
            SurfaceReadErrorCode::ThreadOwnedElsewhere => {
                "task transcript belongs to another thread"
            }
            SurfaceReadErrorCode::ThreadClosed => "task transcript thread is closed",
            SurfaceReadErrorCode::InvalidCursor
            | SurfaceReadErrorCode::StoreUnavailable
            | SurfaceReadErrorCode::RuntimeUnavailable => "task transcript is unavailable",
        }
        .to_string();
        Self {
            code: error.code,
            message,
            current_revision,
        }
    }
}

#[derive(Debug, Clone)]
pub enum UserAction {
    StartSideConversation {
        prompt: Option<String>,
    },
    ToggleSideConversation,
    CloseSideConversation,
    NewSession,
    ForkCurrentSession {
        title: Option<String>,
    },
    RenameCurrentSession {
        title: String,
    },
    ResumeSavedSession {
        session_id: String,
    },
    ForkSavedSession {
        session_id: String,
    },
    RenameSavedSession {
        session_id: String,
        title: String,
    },
    ArchiveSavedSession {
        session_id: String,
    },
    DeleteSavedSession {
        session_id: String,
    },
    Submit(String),
    SubmitWithMentions {
        prompt: String,
        bindings: MentionBindings,
        images: Vec<ComposerImageAttachment>,
    },
    QueuePrompt {
        prompt: String,
        bindings: MentionBindings,
        images: Vec<ComposerImageAttachment>,
    },
    PromptQueueControl(orca_runtime::prompt_queue::PromptQueueAction),
    SubmitQueued {
        id: u64,
        prompt: String,
        bindings: MentionBindings,
        images: Vec<ComposerImageAttachment>,
    },
    PasteImages {
        request_id: u64,
        request: ImagePasteRequest,
    },
    ImplementApprovedPlan {
        prompt: String,
        approval_mode: ApprovalMode,
    },
    SubmitWorkflowNotification(PendingWorkflowNotification),
    RunWorkflow {
        name: String,
        args: Option<String>,
    },
    SetModel(String),
    Remember {
        scope: TuiMemoryScope,
        note: String,
    },
    Compact,
    GoalShow,
    GoalSet(GoalDraft),
    GoalEdit(GoalDraft),
    GoalClear,
    GoalPause,
    GoalResume,
    ResolveBackgroundApproval {
        id: String,
        approved: bool,
    },
    StopTask {
        task_id: String,
    },
    ForegroundTask {
        task_id: String,
    },
    ReadTaskTranscript(TaskTranscriptRequest),
    RespondToInteraction {
        key: TuiInteractionKey,
        response: TuiInteractionResponse,
    },
    Backtrack,
    BackgroundCurrentTurn,
    Interrupt,
    Cancel,
    ResumeOperation {
        operation_id: SurfaceOperationId,
    },
    CancelOperation {
        operation_id: SurfaceOperationId,
    },
}
