use crossbeam_channel as mpsc;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::OperationId;
use orca_core::cost_types::UsageTotals;
use orca_core::goal_types::ThreadGoal;
use orca_core::plan_types::PlanItem;
use orca_core::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};
use orca_core::task_types::BackgroundTaskSummary;
use orca_file_search::{SearchPhase, SearchProgress, SessionGeneration};
use orca_runtime::history::SessionSummary;
use orca_runtime::mentions::{MentionBindings, MentionCandidate};
use orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode;
use orca_runtime::runtime_permission::RuntimePermissionRequestKind;
use orca_runtime::surface::{RuntimeSurfaceThreadHandle, SurfaceOperationId};

use crate::display_text::truncate_to_display_width;
use crate::edit_highlight::EditHighlightState;
#[cfg(test)]
use crate::edit_highlight::parsed_diff_structure_matches_target;
use crate::input_history::load_input_history;
use crate::plan_panel::PlanPanelState;
use crate::queued_input::QueuedSubmissionState;
use crate::streaming_markdown::{StreamingMarkdownAction, StreamingMarkdownAssembler};
#[doc(hidden)]
pub use crate::surface_projection::SurfaceProjectionState;
use crate::surface_projection::{
    SurfaceGoalProjectionEffect, SurfaceGoalProjectionState, SurfaceMetricsState,
    SurfaceOperationProjectionApply, SurfaceOperationProjectionEffect,
    SurfaceOperationProjectionState, SurfaceSessionProjectionApply, SurfaceSessionProjectionEffect,
    SurfaceSessionProjectionState,
};
use crate::transcript_search::TranscriptSearchState;
use crate::transcript_view::TranscriptRenderCache;
#[cfg(test)]
use crate::transcript_view::TranscriptRenderContext;
use crate::workflow_panel::{
    push_pending_workflow_notification_unique, sort_workflow_tasks_for_panel,
};
use crate::workspace_status::GitIdentity;

const SUBAGENT_ACTIVITY_TAIL_LIMIT: usize = 6;
const GOAL_NOTICE_OBJECTIVE_WIDTH: usize = 80;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TuiInteractionKind {
    Approval,
    Permission,
    UserInput,
    McpElicitation,
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

fn format_goal_notice(goal: &orca_core::goal_types::ThreadGoal) -> String {
    use orca_core::goal_types::{
        format_goal_elapsed_seconds, format_tokens_compact, goal_status_label,
    };

    let objective = goal
        .objective
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut parts = vec![
        format!("Goal {}", goal_status_label(goal.status)),
        truncate_to_display_width(&objective, GOAL_NOTICE_OBJECTIVE_WIDTH),
    ];
    if goal.time_used_seconds > 0 {
        parts.push(format_goal_elapsed_seconds(goal.time_used_seconds));
    }
    if let Some(token_budget) = goal.token_budget {
        parts.push(format!(
            "{}/{} tok",
            format_tokens_compact(goal.tokens_used),
            format_tokens_compact(token_budget)
        ));
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiTaskLifecycle {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub turn: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWorkflowNotification {
    pub id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Default)]
pub struct PendingWorkflowNotificationQueue {
    inner: Arc<Mutex<VecDeque<PendingWorkflowNotification>>>,
}

impl PendingWorkflowNotificationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_unique(&self, notification: PendingWorkflowNotification) -> bool {
        let Ok(mut queue) = self.inner.lock() else {
            return false;
        };
        push_pending_workflow_notification_unique(&mut queue, notification)
    }

    pub fn drain_into(&self, target: &mut VecDeque<PendingWorkflowNotification>) {
        let Ok(mut queue) = self.inner.lock() else {
            return;
        };
        while let Some(notification) = queue.pop_front() {
            target.push_back(notification);
        }
    }

    pub fn pop_notification(&self) -> Option<PendingWorkflowNotification> {
        self.inner.lock().ok()?.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .map(|queue| queue.is_empty())
            .unwrap_or(true)
    }

    pub fn clear(&self) {
        if let Ok(mut queue) = self.inner.lock() {
            queue.clear();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|queue| queue.len())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn pop_front(&self) -> Option<PendingWorkflowNotification> {
        self.inner.lock().ok()?.pop_front()
    }
}

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

#[derive(Clone, Debug)]
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
    SurfaceProjectionSynced(Box<SurfaceProjectionState>),
    TurnStarted {
        turn: u32,
        task: Option<TuiTaskLifecycle>,
    },
    QueuedSubmissionStarted {
        id: u64,
    },
    ReasoningDelta(String),
    MessageDelta(String),
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
    WorkflowTasksUpdated {
        tasks: Vec<BackgroundTaskSummary>,
    },
    WorkflowTaskUpdated {
        task: BackgroundTaskSummary,
    },
    BackgroundTaskOutputAttached {
        task_id: String,
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
        mode: RuntimeMcpElicitationMode,
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
    SessionProjectionReset(Box<SurfaceProjectionState>),
    SavedSessionsUpdated {
        sessions: Vec<SessionSummary>,
        next_offset: Option<usize>,
        backfill_complete: bool,
        notice: String,
    },
    SavedSessionActionFailed(String),
    Notice(String),
    MentionSearchDirty {
        generation: SessionGeneration,
    },
    MentionCatalogDirty {
        generation: u64,
    },
    MentionRuntimeReady(RuntimeSurfaceThreadHandle),
    SubmissionRejected {
        queued_id: Option<u64>,
        prompt: String,
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
    GoalStatus(Option<ThreadGoal>),
    Backtracked {
        prompt: String,
    },
}

#[derive(Debug, Clone)]
pub enum TuiMemoryScope {
    User,
    Project,
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
    },
    SubmitQueued {
        id: u64,
        prompt: String,
        bindings: MentionBindings,
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
    GoalSet(String),
    GoalEdit(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Setup,
    SessionPicker,
    Idle,
    Running,
    Compacting,
    WaitingApproval,
    WaitingUserInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideParentStatus {
    Idle,
    Running,
    NeedsApproval,
    NeedsInput,
    Finished,
    Failed,
    Interrupted,
    Closed,
}

impl SideParentStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Idle => "main idle",
            Self::Running => "main running",
            Self::NeedsApproval => "main needs approval",
            Self::NeedsInput => "main needs input",
            Self::Finished => "main finished",
            Self::Failed => "main failed",
            Self::Interrupted => "main interrupted",
            Self::Closed => "main closed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SideConversationUiState {
    pub(crate) parent_thread_id: String,
    pub(crate) parent_title: String,
    pub(crate) parent_status: SideParentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerPhase {
    Browsing,
    Actions {
        session_id: String,
        selected: usize,
    },
    Renaming {
        session_id: String,
        value: String,
    },
    ConfirmArchive {
        session_id: String,
        title: String,
        selected: usize,
    },
    ConfirmDelete {
        session_id: String,
        title: String,
        selected: usize,
    },
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    Reasoning(String),
    Assistant(String),
    AssistantChunk {
        text: String,
        trailing_blank: bool,
    },
    ProposedPlan(String),
    ToolCall {
        id: String,
        name: String,
        target: Option<String>,
        status: String,
        output: Option<String>,
        diff: Option<String>,
        kind: Option<String>,
        expanded: bool,
    },
    PlanUpdate {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    Subagent {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
        activity: Option<String>,
        activity_tail: Vec<String>,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
        expanded: bool,
    },
    Error(String),
    System(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalDialog {
    pub plan: String,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOption {
    /// Approve this single call.
    Once,
    /// Approve and remember this tool for the rest of the session.
    AlwaysTool,
    /// Approve and remember this tool + target for the rest of the session.
    AlwaysTarget,
    /// Reject this call.
    Deny,
}

impl ApprovalOption {
    pub fn key(self) -> char {
        match self {
            ApprovalOption::Once => '1',
            ApprovalOption::AlwaysTarget => '2',
            ApprovalOption::AlwaysTool => '3',
            ApprovalOption::Deny => '4',
        }
    }

    pub fn legacy_key(self) -> char {
        match self {
            ApprovalOption::Once => 'y',
            ApprovalOption::AlwaysTool => 'a',
            ApprovalOption::AlwaysTarget => 'A',
            ApprovalOption::Deny => 'n',
        }
    }

    pub fn matches_key(self, key: char) -> bool {
        key == self.key() || key == self.legacy_key()
    }

    pub fn label(self) -> &'static str {
        match self {
            ApprovalOption::Once => "allow this once",
            ApprovalOption::AlwaysTool => "always allow",
            ApprovalOption::AlwaysTarget => "always allow this exact call",
            ApprovalOption::Deny => "deny",
        }
    }

    /// Whether choosing this option lets the tool run.
    pub fn is_approve(self) -> bool {
        !matches!(self, ApprovalOption::Deny)
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub id: String,
    pub interaction: Option<TuiInteractionKey>,
    pub tool: String,
    pub target: Option<String>,
    pub permission_kind: Option<RuntimePermissionRequestKind>,
    pub background_task_id: Option<String>,
    pub selected: usize,
    pub options: Vec<ApprovalOption>,
    pub diff: Option<String>,
}

impl ApprovalDialog {
    /// Tools whose target is inherently dynamic (e.g. search queries) —
    /// the "always allow this exact call" option would never match again,
    /// so we hide it to reduce noise.
    const DYNAMIC_TARGET_TOOLS: &[&str] = &["web_search", "search", "grep"];

    /// Returns the set of options to display. The `AlwaysTarget` option is
    /// only shown when a target is present AND the tool is likely to be
    /// called again with the same target (e.g. reading a fixed file path).
    pub fn options_for(tool: &str, target: Option<&str>) -> Vec<ApprovalOption> {
        let show_always_target =
            target.is_some() && !Self::DYNAMIC_TARGET_TOOLS.iter().any(|t| tool.contains(t));

        if show_always_target {
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        } else {
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        }
    }

    pub fn current(&self) -> ApprovalOption {
        self.options
            .get(self.selected)
            .copied()
            .unwrap_or(ApprovalOption::Deny)
    }

    pub fn option_for_key(&self, key: char) -> Option<ApprovalOption> {
        self.options
            .iter()
            .copied()
            .find(|option| option.matches_key(key))
    }

    pub fn title(&self) -> &'static str {
        match self.permission_kind {
            Some(RuntimePermissionRequestKind::NetworkBlock) => " Network Permission Required ",
            Some(RuntimePermissionRequestKind::FilesystemWrite) => {
                " Filesystem Permission Required "
            }
            Some(RuntimePermissionRequestKind::UnsandboxedShellRetry) => {
                " Unsandboxed Shell Required "
            }
            None => " Approval Required ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    Conversation,
    Workflows,
    Agents,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowPanelState {
    pub selected: usize,
    pub tasks: Vec<BackgroundTaskSummary>,
}

#[derive(Debug, Clone)]
pub struct SlashMenuItem {
    pub command: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SlashMenu {
    pub items: Vec<SlashMenuItem>,
    pub selected: usize,
    pub sub_menu: Option<SubMenu>,
}

#[derive(Debug, Clone)]
pub struct SubMenu {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    /// Carries a value chosen in an earlier step of a multi-step picker (e.g. the
    /// model picked in step 1 of `/model`, while step 2 asks for reasoning effort).
    /// Nothing is applied until the final step confirms, so Esc cancels cleanly.
    pub context: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MentionPopupState {
    pub candidates: Vec<MentionCandidate>,
    pub selected: usize,
    pub selected_identity: Option<String>,
    pub manual_selection: bool,
    pub sigil: Option<orca_runtime::mentions::MentionSigil>,
    pub phase: Option<SearchPhase>,
    pub progress: SearchProgress,
    pub pending_query: Option<String>,
    pub dismissed_query: Option<String>,
}

impl MentionPopupState {
    pub(crate) fn clear_projection(&mut self) {
        self.candidates.clear();
        self.selected = 0;
        self.selected_identity = None;
        self.manual_selection = false;
        self.phase = None;
        self.progress = SearchProgress::default();
        self.pending_query = None;
    }
}

pub struct AppState {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) message_revisions: Vec<u64>,
    tool_call_indices: HashMap<String, usize>,
    next_message_revision: u64,
    pub(crate) transcript_render_cache: TranscriptRenderCache,
    pub(crate) transcript_search: TranscriptSearchState,
    /// Separate cache for the welcome screen (no messages yet), so its text
    /// is selectable/copyable through the same machinery as the transcript.
    pub(crate) welcome_render_cache: TranscriptRenderCache,
    /// Watermark splitting finished turns from the current turn. Streaming appends target
    /// the live suffix, but historical tool/subagent expansion can still mutate an older
    /// message and must advance that message's render revision.
    pub finalized_count: usize,
    /// How many messages are omitted from the live transcript renderer. This is zero in the
    /// current fullscreen TUI, but remains part of the state model for older inline-viewport
    /// behavior and tests that exercise finalized/live suffix boundaries.
    pub flushed_count: usize,
    pub status: AppStatus,
    pub running_started_at: Option<Instant>,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub total_lines: usize,
    pub visible_height: usize,
    pub app_version: String,
    pub model_name: String,
    pub reasoning_effort: orca_core::config::ReasoningEffort,
    pub approval_mode: ApprovalMode,
    pub pre_plan_approval_mode: Option<ApprovalMode>,
    pub cwd: String,
    pub(crate) surface_session: SurfaceSessionProjectionState,
    pub(crate) side_conversation: Option<SideConversationUiState>,
    pub(crate) side_conversation_visible: bool,
    pub(crate) active_session_attachment: Option<SessionAttachmentId>,
    pub(crate) workspace_git: Option<GitIdentity>,
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_dialog: Option<ApprovalDialog>,
    pub plan_approval_dialog: Option<PlanApprovalDialog>,
    pub pending_input: Option<PendingTuiInput>,
    /// Tool / "tool\u{0}target" keys the user chose to always allow this
    /// session. Checked when a new approval arrives so the dialog is skipped.
    pub approval_allowlist: std::collections::HashSet<String>,
    pub setup_step: u8,
    pub show_shortcuts: bool,
    pub input_history: Vec<String>,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub(crate) queued_submission: QueuedSubmissionState,
    pub history_cursor: Option<usize>,
    pub draft_before_history: Option<String>,
    pub last_ctrl_c: Option<Instant>,
    pub last_completed_at: Option<Instant>,
    pub session_picker_sessions: Vec<SessionSummary>,
    pub session_picker_selected: usize,
    pub session_picker_query: String,
    pub session_picker_phase: SessionPickerPhase,
    pub session_picker_error: Option<String>,
    pub session_picker_next_offset: Option<usize>,
    pub session_picker_backfill_complete: bool,
    pub(crate) surface_metrics: SurfaceMetricsState,
    pub slash_menu: Option<SlashMenu>,
    pub mention: MentionPopupState,
    pub mention_bindings: MentionBindings,
    pub atomic_skill_tokens: MentionBindings,
    pub(crate) plan_panel: PlanPanelState,
    proposed_plan_parser: ProposedPlanStreamParser,
    assistant_stream: StreamingMarkdownAssembler,
    assistant_stream_tail: Option<usize>,
    pub(crate) surface_goal: SurfaceGoalProjectionState,
    pub(crate) surface_operation: SurfaceOperationProjectionState,
    pub recovery_prompt_visible: bool,
    pub recovery_prompt_selected: usize,
    pub panel_mode: PanelMode,
    pub workflow_panel: WorkflowPanelState,
    pub pending_workflow_notifications: VecDeque<PendingWorkflowNotification>,
    pub suppress_background_main_session_output: bool,
    pub tick: u64,
    /// Mouse drag selection over the transcript, anchored in content space.
    pub selection: Option<crate::selection::TranscriptSelection>,
    /// Screen rect of the transcript on the last frame; maps mouse to content.
    pub transcript_area: Option<ratatui::layout::Rect>,
    /// Absolute visual row of the transcript area's first line last frame.
    pub viewport_base_row: usize,
    /// Selected text awaiting a clipboard write by the app loop.
    pub pending_clipboard_copy: Option<String>,
    /// Last left-button press: time, cell, and click count (1 = single,
    /// 2 = double, 3 = triple; further quick clicks cycle back to 1).
    pub last_left_click: Option<(Instant, u16, u16, u8)>,
    /// Transient "copied N chars" feedback on the status line.
    pub copy_notice: Option<CopyNotice>,
    /// Active edge-drag auto-scroll: direction (-1 up / +1 down) + pointer
    /// column. Set while a drag sits on the transcript's first/last row and
    /// applied on every animation tick, so scrolling continues even when the
    /// pointer stops moving (terminals only send drag events on movement).
    pub drag_edge_scroll: Option<(i8, u16)>,
    /// Screen rect of the floating "Jump to bottom" pill on the last frame;
    /// `None` while auto-follow is on (the pill only shows when scrolled up).
    pub jump_to_bottom_area: Option<ratatui::layout::Rect>,
    /// Full frame rect from the last render, for popup hit-testing
    /// (approval dialog, session picker).
    pub frame_area: Option<ratatui::layout::Rect>,
    /// Composer (input box) outer rect from the last render, `None` while
    /// the composer is hidden.
    pub input_area: Option<ratatui::layout::Rect>,
    pub(crate) search_area: Option<ratatui::layout::Rect>,
    pub(crate) edit_highlights: EditHighlightState,
    /// A mouse drag is adjusting the composer's own text selection.
    pub composer_mouse_selecting: bool,
    /// Messages that arrived below the viewport while auto-follow was
    /// disarmed — the jump pill's "N new messages" unread count. Streaming
    /// deltas rewrite the tail message and do NOT count; only message
    /// boundaries do.
    pub unseen_messages: usize,
}

pub trait ScrollAmount {
    fn as_usize(self) -> usize;
}

/// Transient status-line feedback after a mouse copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyNotice {
    pub chars: usize,
    pub at: Instant,
    /// Too large for OSC 52 — only the local helper (pbcopy/wl-copy/xclip)
    /// received the text, so remote/SSH clipboards were not updated.
    pub local_only: bool,
}

impl ScrollAmount for usize {
    fn as_usize(self) -> usize {
        self
    }
}

impl ScrollAmount for u16 {
    fn as_usize(self) -> usize {
        self as usize
    }
}

impl ScrollAmount for u32 {
    fn as_usize(self) -> usize {
        self as usize
    }
}

impl ScrollAmount for i32 {
    fn as_usize(self) -> usize {
        self.max(0) as usize
    }
}

impl AppState {
    pub(crate) fn side_conversation_active(&self) -> bool {
        self.side_conversation_visible
    }

    pub(crate) fn side_conversation_available(&self) -> bool {
        self.side_conversation.is_some()
    }

    pub fn new(
        event_tx: mpsc::Sender<UserAction>,
        app_version: String,
        model_name: String,
        cwd: String,
    ) -> Self {
        Self {
            messages: Vec::new(),
            message_revisions: Vec::new(),
            tool_call_indices: HashMap::new(),
            next_message_revision: 1,
            transcript_render_cache: TranscriptRenderCache::default(),
            transcript_search: TranscriptSearchState::default(),
            welcome_render_cache: TranscriptRenderCache::default(),
            finalized_count: 0,
            flushed_count: 0,
            status: AppStatus::Idle,
            running_started_at: None,
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            visible_height: 0,
            app_version,
            model_name,
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            approval_mode: ApprovalMode::default(),
            pre_plan_approval_mode: None,
            cwd,
            surface_session: SurfaceSessionProjectionState::default(),
            side_conversation: None,
            side_conversation_visible: false,
            active_session_attachment: None,
            workspace_git: None,
            event_tx,
            approval_dialog: None,
            plan_approval_dialog: None,
            pending_input: None,
            approval_allowlist: std::collections::HashSet::new(),
            setup_step: 0,
            show_shortcuts: false,
            input_history: load_input_history(),
            pending_pastes: Vec::new(),
            queued_submission: QueuedSubmissionState::default(),
            history_cursor: None,
            draft_before_history: None,
            last_ctrl_c: None,
            last_completed_at: None,
            session_picker_sessions: Vec::new(),
            session_picker_selected: 0,
            session_picker_query: String::new(),
            session_picker_phase: SessionPickerPhase::Browsing,
            session_picker_error: None,
            session_picker_next_offset: None,
            session_picker_backfill_complete: true,
            surface_metrics: SurfaceMetricsState::default(),
            slash_menu: None,
            mention: MentionPopupState::default(),
            mention_bindings: MentionBindings::default(),
            atomic_skill_tokens: MentionBindings::default(),
            plan_panel: PlanPanelState::default(),
            proposed_plan_parser: ProposedPlanStreamParser::default(),
            assistant_stream: StreamingMarkdownAssembler::default(),
            assistant_stream_tail: None,
            surface_goal: SurfaceGoalProjectionState::default(),
            surface_operation: SurfaceOperationProjectionState::default(),
            recovery_prompt_visible: false,
            recovery_prompt_selected: 0,
            panel_mode: PanelMode::Conversation,
            workflow_panel: WorkflowPanelState::default(),
            pending_workflow_notifications: VecDeque::new(),
            suppress_background_main_session_output: false,
            tick: 0,
            selection: None,
            transcript_area: None,
            viewport_base_row: 0,
            pending_clipboard_copy: None,
            last_left_click: None,
            copy_notice: None,
            drag_edge_scroll: None,
            jump_to_bottom_area: None,
            frame_area: None,
            input_area: None,
            search_area: None,
            edit_highlights: EditHighlightState::default(),
            composer_mouse_selecting: false,
            unseen_messages: 0,
        }
    }

    /// Map a mouse position to transcript content space; `None` outside the
    /// transcript area (or before the first frame rendered it).
    pub(crate) fn transcript_pos_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::selection::SelectionPos> {
        let area = self.transcript_area?;
        crate::selection::screen_to_selection_pos(area, self.viewport_base_row, column, row)
    }

    /// Like [`Self::transcript_pos_at`] but clamps to the nearest transcript
    /// cell, so drags that leave the area keep tracking.
    pub(crate) fn transcript_pos_at_clamped(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::selection::SelectionPos> {
        let area = self.transcript_area?;
        crate::selection::screen_to_selection_pos_clamped(area, self.viewport_base_row, column, row)
    }

    /// The transcript cache mouse interaction should read from: the welcome
    /// cache while no messages exist, the live transcript cache afterwards.
    pub(crate) fn active_transcript_cache(&self) -> &TranscriptRenderCache {
        if self.messages.is_empty() {
            &self.welcome_render_cache
        } else {
            &self.transcript_render_cache
        }
    }

    pub(crate) fn extract_selection_text(
        &self,
        selection: &crate::selection::TranscriptSelection,
    ) -> String {
        self.active_transcript_cache().extract_text(selection)
    }

    pub(crate) fn selection_word_bounds(
        &self,
        pos: crate::selection::SelectionPos,
    ) -> Option<(
        crate::selection::SelectionPos,
        crate::selection::SelectionPos,
    )> {
        self.active_transcript_cache().word_bounds_at(pos)
    }

    pub(crate) fn selection_line_bounds(
        &self,
        pos: crate::selection::SelectionPos,
    ) -> Option<(
        crate::selection::SelectionPos,
        crate::selection::SelectionPos,
    )> {
        self.active_transcript_cache().line_bounds_at(pos)
    }

    /// How long the "copied N chars" notice stays on the status line.
    pub(crate) const COPY_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(4);

    /// Stage `text` for the clipboard and start the status-line notice.
    pub(crate) fn stage_clipboard_copy(&mut self, text: String, now: Instant) {
        self.copy_notice = Some(CopyNotice {
            chars: text.chars().count(),
            at: now,
            // Terminals cap OSC 52 length; the app loop will skip the remote
            // write for oversized text, so the notice must not overclaim.
            local_only: text.len() > crate::clipboard::OSC52_MAX_TEXT_BYTES,
        });
        self.pending_clipboard_copy = Some(text);
    }

    /// The staged copy notice while it is still fresh.
    pub fn copy_notice_at(&self, now: Instant) -> Option<CopyNotice> {
        self.copy_notice
            .filter(|notice| now.duration_since(notice.at) < Self::COPY_NOTICE_TTL)
    }

    /// One animation-tick step of edge-drag auto-scroll: scroll a line and
    /// grow the selection head one content row in the drag direction. The
    /// head moves in content space, so it stays glued to what it selected.
    pub(crate) fn apply_drag_edge_scroll(&mut self) {
        let Some((direction, column)) = self.drag_edge_scroll else {
            return;
        };
        let col = self
            .transcript_area
            .map(|area| column.saturating_sub(area.x) as usize)
            .unwrap_or(0);
        let Some(mut selection) = self.selection.filter(|selection| selection.dragging) else {
            return;
        };
        if direction < 0 {
            self.scroll_up(1usize);
            selection.head.row = selection.head.row.saturating_sub(1);
        } else {
            self.scroll_down(1usize);
            let last_row = self.total_lines.saturating_sub(1);
            selection.head.row = selection.head.row.saturating_add(1).min(last_row);
        }
        selection.head.col = col;
        self.selection = Some(selection);
    }

    /// Drop the mouse selection (and any armed edge auto-scroll).
    ///
    /// Called whenever the transcript's visual row space changes shape —
    /// terminal resize (re-wrap), message removal/replacement, or an in-place
    /// rewrite of a non-tail message. Positions kept from the old row space
    /// would highlight and copy unrelated content.
    pub(crate) fn invalidate_selection(&mut self) {
        self.selection = None;
        self.drag_edge_scroll = None;
    }

    fn allocate_message_revision(&mut self) -> u64 {
        let revision = self.next_message_revision;
        self.next_message_revision = self.next_message_revision.wrapping_add(1).max(1);
        revision
    }

    fn push_goal_notice(&mut self, notice: String) {
        // The live Goal banner is the source of truth for current status, so the
        // transcript only needs a notice when the rendered line actually changes.
        // Collapsing consecutive identical notices keeps the periodic refreshes
        // emitted between auto-continuation turns (which land while the app is
        // Idle) from stacking duplicate lines.
        let duplicate = self
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                ChatMessage::System(text) if text.starts_with("Goal ") => Some(text),
                _ => None,
            })
            == Some(&notice);
        if !duplicate {
            self.finish_assistant_stream();
            self.push_message(ChatMessage::System(notice));
        }
    }

    pub(crate) fn reconcile_message_tracking(&mut self) {
        let structure_changed = self.message_revisions.len() != self.messages.len();
        if self.message_revisions.len() > self.messages.len() {
            self.message_revisions.truncate(self.messages.len());
            self.transcript_render_cache.truncate(self.messages.len());
            self.invalidate_selection();
        }
        while self.message_revisions.len() < self.messages.len() {
            let revision = self.allocate_message_revision();
            self.message_revisions.push(revision);
        }
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
        if structure_changed {
            self.rebuild_tool_call_indices();
            self.assert_tool_call_index_consistent();
        }
    }

    fn reset_message_tracking(&mut self) {
        self.message_revisions.clear();
        self.transcript_render_cache.clear();
        while self.message_revisions.len() < self.messages.len() {
            let revision = self.allocate_message_revision();
            self.message_revisions.push(revision);
        }
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
        self.rebuild_tool_call_indices();
        self.assert_tool_call_index_consistent();
    }

    fn rebuild_tool_call_indices(&mut self) {
        self.tool_call_indices.clear();
        for (index, message) in self.messages.iter().enumerate() {
            if let ChatMessage::ToolCall { id, .. } = message {
                self.tool_call_indices.entry(id.clone()).or_insert(index);
            }
        }
    }

    fn tool_call_message_index(&self, id: &str) -> Option<usize> {
        self.tool_call_indices.get(id).copied()
    }

    fn receiving_tool_call_message_index(&self, id: &str) -> Option<usize> {
        let is_receiving = |message: &ChatMessage| {
            matches!(
                message,
                ChatMessage::ToolCall {
                    id: existing_id,
                    status,
                    ..
                } if existing_id == id && status == "receiving"
            )
        };
        let first = self.tool_call_message_index(id)?;
        let first_message = self.messages.get(first)?;
        if is_receiving(first_message) {
            return Some(first);
        }
        self.messages
            .get(first + 1..)?
            .iter()
            .rposition(is_receiving)
            .map(|offset| first + 1 + offset)
    }

    #[cfg(any(test, debug_assertions))]
    fn assert_tool_call_index_consistent(&self) {
        let mut canonical = HashMap::new();
        for (index, message) in self.messages.iter().enumerate() {
            if let ChatMessage::ToolCall { id, .. } = message {
                canonical.entry(id.clone()).or_insert(index);
            }
        }
        debug_assert_eq!(self.tool_call_indices, canonical);
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn assert_tool_call_index_consistent(&self) {}

    fn apply_surface_projection_state(&mut self, projection: SurfaceProjectionState) {
        let mut surface_session = self.surface_session.clone();
        let session_apply = surface_session.apply_projection(&projection);
        let mut surface_operation = self.surface_operation.clone();
        let operation_apply = surface_operation.apply_projection(&projection);
        if matches!(session_apply, SurfaceSessionProjectionApply::Rejected)
            || matches!(operation_apply, SurfaceOperationProjectionApply::Rejected)
            || self
                .surface_metrics
                .rejects_usage_revision(projection.usage_revision)
        {
            return;
        }
        self.surface_session = surface_session;
        self.surface_operation = surface_operation;
        self.surface_metrics.apply_projection(&projection);
        let goal_effect = self.surface_goal.apply_projection(&projection);
        self.apply_workflow_tasks_update(projection.workflow_tasks.clone());
        match operation_apply {
            SurfaceOperationProjectionApply::Rejected => unreachable!("projection was rejected"),
            SurfaceOperationProjectionApply::Accepted(operation_effect) => match operation_effect {
                Some(SurfaceOperationProjectionEffect::RecoveryPromptShown) => {
                    self.recovery_prompt_visible = true;
                    self.recovery_prompt_selected = 0;
                    self.push_message(ChatMessage::System(
                    "A recoverable operation is suspended. Use the recovery controls to continue it or /cancel-operation to close it."
                        .to_string(),
                ));
                }
                Some(SurfaceOperationProjectionEffect::RecoveryPromptCleared) => {
                    self.recovery_prompt_visible = false;
                    self.recovery_prompt_selected = 0;
                }
                None => {}
            },
        }
        match goal_effect {
            Some(SurfaceGoalProjectionEffect::Updated(goal)) => {
                let should_keep_running =
                    self.status == AppStatus::Running && goal.status.should_continue();
                let notice = format_goal_notice(&goal);
                self.push_goal_notice(notice);
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            Some(SurfaceGoalProjectionEffect::Cleared) => {
                self.finish_assistant_stream();
                self.push_message(ChatMessage::System("Goal cleared.".to_string()));
                self.set_status(AppStatus::Idle);
            }
            None => {}
        }
        match session_apply {
            SurfaceSessionProjectionApply::Accepted(Some(
                SurfaceSessionProjectionEffect::Renamed { title },
            )) => {
                self.push_message(ChatMessage::System(format!(
                    "Renamed conversation to {title}."
                )));
                self.set_status(AppStatus::Idle);
            }
            SurfaceSessionProjectionApply::Accepted(Some(
                SurfaceSessionProjectionEffect::Forked { title },
            )) => {
                self.push_message(ChatMessage::System(format!(
                    "Forked conversation as {title}."
                )));
                self.set_status(AppStatus::Idle);
            }
            SurfaceSessionProjectionApply::Accepted(None)
            | SurfaceSessionProjectionApply::Rejected => {}
        }
    }

    #[cfg(any(test, debug_assertions))]
    fn assert_surface_projection_consistent(&self, projection: &SurfaceProjectionState) {
        self.surface_session.assert_matches_projection(projection);
        self.surface_metrics.assert_matches_projection(projection);
        debug_assert_eq!(
            self.workflow_panel.tasks,
            sort_workflow_tasks_for_panel(projection.workflow_tasks.clone())
        );
        self.surface_goal.assert_matches_projection(projection);
        self.surface_operation.assert_matches_projection(projection);
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn assert_surface_projection_consistent(&self, _projection: &SurfaceProjectionState) {}

    pub(crate) fn push_message(&mut self, message: ChatMessage) {
        self.reconcile_message_tracking();
        if let ChatMessage::ToolCall { id, .. } = &message {
            let reused_tool_id = self.tool_call_message_index(id).is_some();
            self.remove_applied_highlights_for_tool_id(id);
            if reused_tool_id {
                self.clear_pending_edit_highlights();
            }
        }
        let revision = self.allocate_message_revision();
        if let ChatMessage::ToolCall { id, .. } = &message {
            self.tool_call_indices
                .entry(id.clone())
                .or_insert(self.messages.len());
        }
        self.messages.push(message);
        self.message_revisions.push(revision);
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
        // A message landed below the viewport while the user was scrolled
        // up: feed the jump pill's unread count.
        if !self.auto_scroll {
            self.unseen_messages = self.unseen_messages.saturating_add(1);
        }
    }

    pub(crate) fn replace_messages(&mut self, messages: impl IntoIterator<Item = ChatMessage>) {
        self.reset_assistant_stream();
        self.reset_queued_user_messages();
        self.messages = messages.into_iter().collect();
        self.clear_applied_edit_highlights();
        self.clear_pending_edit_highlights();
        self.reset_message_tracking();
        self.finalized_count = 0;
        self.flushed_count = 0;
        self.unseen_messages = 0;
        self.invalidate_selection();
    }

    pub(crate) fn clear_messages(&mut self) {
        self.reset_assistant_stream();
        self.reset_queued_user_messages();
        self.transcript_search.reset();
        self.messages.clear();
        self.message_revisions.clear();
        self.tool_call_indices.clear();
        self.transcript_render_cache.clear();
        self.clear_applied_edit_highlights();
        self.clear_pending_edit_highlights();
        self.finalized_count = 0;
        self.flushed_count = 0;
        self.unseen_messages = 0;
        self.invalidate_selection();
    }

    pub(crate) fn reset_session_projection(&mut self) {
        self.surface_session.reset();
        self.clear_messages();
        self.clear_plan_panel();
        self.proposed_plan_parser = ProposedPlanStreamParser::default();
        self.surface_goal.reset();
        self.surface_operation.reset();
        self.recovery_prompt_visible = false;
        self.recovery_prompt_selected = 0;
        self.surface_metrics.reset();
        self.approval_dialog = None;
        self.pending_input = None;
        self.approval_allowlist.clear();
        self.session_picker_sessions.clear();
        self.session_picker_selected = 0;
        self.session_picker_query.clear();
        self.session_picker_phase = SessionPickerPhase::Browsing;
        self.session_picker_error = None;
        self.slash_menu = None;
        self.mention = MentionPopupState::default();
        self.mention_bindings.clear();
        self.atomic_skill_tokens.clear();
        self.plan_approval_dialog = None;
        self.pre_plan_approval_mode = None;
        self.pending_pastes.clear();
        self.reset_history_navigation();
        self.last_ctrl_c = None;
        self.panel_mode = PanelMode::Conversation;
        self.workflow_panel = WorkflowPanelState::default();
        self.pending_workflow_notifications.clear();
        self.suppress_background_main_session_output = false;
        self.last_completed_at = None;
        self.pending_clipboard_copy = None;
        self.last_left_click = None;
        self.copy_notice = None;
        self.composer_mouse_selecting = false;
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.set_status(AppStatus::Idle);
    }

    pub(crate) fn truncate_messages(&mut self, len: usize) {
        if self
            .assistant_stream_tail
            .is_none_or(|tail_index| tail_index >= len)
        {
            self.reset_assistant_stream();
        }
        self.reconcile_message_tracking();
        let did_truncate = len < self.messages.len();
        if did_truncate {
            self.invalidate_selection();
            self.clear_pending_edit_highlights();
        }
        self.messages.truncate(len);
        self.message_revisions.truncate(len);
        self.transcript_render_cache.truncate(len);
        self.finalized_count = self.finalized_count.min(len);
        self.flushed_count = self.flushed_count.min(len);
        self.prune_applied_diff_highlights();
        if did_truncate {
            self.rebuild_tool_call_indices();
        }
    }

    pub(crate) fn replace_message(&mut self, index: usize, message: ChatMessage) -> bool {
        self.reconcile_message_tracking();
        if index >= self.messages.len() {
            return false;
        }
        self.remove_applied_highlight_for_message(index);
        let previous_tool_id = self.messages.get(index).and_then(|message| match message {
            ChatMessage::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        });
        let next_tool_id = match &message {
            ChatMessage::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.messages[index] = message;
        if previous_tool_id != next_tool_id {
            self.rebuild_tool_call_indices();
        }
        self.touch_message(index);
        true
    }

    pub(crate) fn mutate_message<R>(
        &mut self,
        index: usize,
        mutate: impl FnOnce(&mut ChatMessage) -> R,
    ) -> Option<R> {
        self.reconcile_message_tracking();
        self.remove_applied_highlight_for_message(index);
        let previous_tool_id = self.messages.get(index).and_then(|message| match message {
            ChatMessage::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        });
        let result = mutate(self.messages.get_mut(index)?);
        let current_tool_id = self.messages.get(index).and_then(|message| match message {
            ChatMessage::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        });
        if previous_tool_id != current_tool_id {
            self.rebuild_tool_call_indices();
        }
        self.touch_message(index);
        Some(result)
    }

    pub(crate) fn touch_message(&mut self, index: usize) -> bool {
        self.reconcile_message_tracking();
        if index >= self.message_revisions.len() {
            return false;
        }
        let old_revision = self.message_revisions[index];
        self.cancel_pending_edit_highlight_for_message(index, old_revision);
        self.remove_applied_highlight_for_message(index);
        // Rewriting any message but the tail can change its height and shift
        // every row below it. Tail rewrites (streaming deltas) leave earlier
        // rows in place, so a selection above the stream stays valid.
        if index + 1 != self.messages.len() {
            self.invalidate_selection();
        }
        let revision = self.allocate_message_revision();
        self.message_revisions[index] = revision;
        self.transcript_render_cache.invalidate(index);
        true
    }

    pub(crate) fn retain_messages(&mut self, mut keep: impl FnMut(&ChatMessage) -> bool) {
        self.reconcile_message_tracking();
        let messages = std::mem::take(&mut self.messages);
        let revisions = std::mem::take(&mut self.message_revisions);
        let active_tail = self.assistant_stream_tail;
        let mut retained_tail = None;
        let finalized_count = self.finalized_count.min(messages.len());
        let flushed_count = self.flushed_count.min(messages.len());
        let mut retained_finalized = 0;
        let mut retained_flushed = 0;
        let mut retained_mask = Vec::with_capacity(messages.len());
        let mut removed_tool_revisions = Vec::new();
        for (index, (message, revision)) in messages.into_iter().zip(revisions).enumerate() {
            let retain = keep(&message);
            retained_mask.push(retain);
            if retain {
                if active_tail == Some(index) {
                    retained_tail = Some(self.messages.len());
                }
                retained_finalized += usize::from(index < finalized_count);
                retained_flushed += usize::from(index < flushed_count);
                self.messages.push(message);
                self.message_revisions.push(revision);
            } else if matches!(message, ChatMessage::ToolCall { .. }) {
                removed_tool_revisions.push(revision);
            }
        }
        self.transcript_render_cache.retain(&retained_mask);
        self.rebuild_tool_call_indices();
        if active_tail.is_some() {
            if retained_tail.is_some() {
                self.assistant_stream_tail = retained_tail;
            } else {
                self.reset_assistant_stream();
            }
        }
        self.finalized_count = retained_finalized;
        self.flushed_count = retained_flushed;
        if retained_mask.iter().any(|retain| !retain) {
            for revision in removed_tool_revisions {
                self.remove_applied_highlight_revision(revision);
            }
            self.clear_pending_edit_highlights();
            self.prune_applied_diff_highlights();
            self.invalidate_selection();
        }
        self.assert_tool_call_index_consistent();
    }

    pub fn enter_running(&mut self) {
        self.plan_approval_dialog = None;
        if self.running_started_at.is_none() {
            self.running_started_at = Some(Instant::now());
        }
        self.status = AppStatus::Running;
    }

    pub fn set_status(&mut self, status: AppStatus) {
        if status == AppStatus::Running {
            self.enter_running();
        } else if matches!(
            status,
            AppStatus::Compacting | AppStatus::WaitingApproval | AppStatus::WaitingUserInput
        ) {
            self.status = status;
        } else {
            self.status = status;
            self.running_started_at = None;
        }
    }

    pub fn scroll_up(&mut self, lines: impl ScrollAmount) {
        let lines = lines.as_usize();
        // With everything already on screen there is nothing to scroll: a wheel tick
        // here (trackpad inertia, an accidental touch) must not silently unpin
        // auto-follow — the view wouldn't move, so the user gets no feedback that
        // follow was disarmed, and the transcript then stops tracking new content
        // the moment it grows past one screen.
        if self.total_lines <= self.visible_height {
            return;
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, lines: impl ScrollAmount) {
        let lines = lines.as_usize();
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(max_scroll);
        if self.scroll_offset >= max_scroll {
            self.auto_scroll = true;
            // Back at the tail: nothing below is unseen any more.
            self.unseen_messages = 0;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        self.scroll_offset = max_scroll;
        self.auto_scroll = true;
        self.unseen_messages = 0;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = false;
    }

    pub fn accepts_mouse_scroll_at(&self, now: Instant) -> bool {
        const COMPLETION_MOUSE_SCROLL_GRACE: std::time::Duration =
            std::time::Duration::from_millis(800);
        self.last_completed_at.is_none_or(|completed_at| {
            now.duration_since(completed_at) >= COMPLETION_MOUSE_SCROLL_GRACE
        })
    }

    pub fn toggle_shortcuts(&mut self) {
        self.show_shortcuts = !self.show_shortcuts;
    }

    pub fn advance_tick(&mut self) {
        if self.status == AppStatus::Running {
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn toggle_latest_tool_output(&mut self) -> bool {
        // Only the live pane is mutable and re-renderable. Anything below `flushed_count`
        // has been committed to the terminal's immutable scrollback (in fully-expanded
        // form), so `e` can only toggle a live tool/subagent message.
        let live_start = self.flushed_count.min(self.messages.len());
        let Some(index) = self.messages[live_start..].iter().rposition(|message| {
            matches!(
                message,
                ChatMessage::ToolCall { .. } | ChatMessage::Subagent { .. }
            )
        }) else {
            return false;
        };
        self.mutate_message(live_start + index, |message| match message {
            ChatMessage::ToolCall { expanded, .. } | ChatMessage::Subagent { expanded, .. } => {
                *expanded = !*expanded;
            }
            _ => unreachable!(),
        });
        true
    }
}

impl AppState {
    pub fn nth_final_assistant_response(&self, position: usize) -> Option<&str> {
        if position == 0 {
            return None;
        }
        self.messages
            .iter()
            .rev()
            .filter_map(|message| match message {
                ChatMessage::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .nth(position - 1)
    }

    /// Allowlist key for a tool alone.
    pub fn approval_key_tool(tool: &str) -> String {
        tool.to_string()
    }

    /// Allowlist key for a tool scoped to a specific target.
    pub fn approval_key_target(tool: &str, target: &str) -> String {
        format!("{tool}\u{0}{target}")
    }

    /// True if a pending approval for this tool/target was already granted an
    /// "always allow" this session.
    pub fn approval_is_allowlisted(&self, tool: &str, target: Option<&str>) -> bool {
        if self
            .approval_allowlist
            .contains(&Self::approval_key_tool(tool))
        {
            return true;
        }
        if let Some(target) = target {
            return self
                .approval_allowlist
                .contains(&Self::approval_key_target(tool, target));
        }
        false
    }

    pub fn update(&mut self, event: TuiEvent) {
        self.reconcile_message_tracking();
        match event {
            TuiEvent::Attached(_) => {
                eprintln!("orca: ignored an attached TUI event that bypassed attachment fencing");
            }
            TuiEvent::SessionAttachmentActivated => {}
            TuiEvent::SideConversationChanged {
                active,
                available,
                parent_thread_id,
                parent_title,
                parent_status,
            } => {
                if available {
                    self.side_conversation = Some(SideConversationUiState {
                        parent_thread_id,
                        parent_title,
                        parent_status,
                    });
                } else {
                    self.side_conversation = None;
                }
                self.side_conversation_visible = active;
            }
            TuiEvent::SideParentStatusChanged(status) => {
                if let Some(side) = self.side_conversation.as_mut() {
                    side.parent_status = status;
                }
            }
            TuiEvent::SurfaceProjectionSynced(projection) => {
                self.apply_surface_projection_state(*projection);
            }
            TuiEvent::NewSessionStarted => {}
            TuiEvent::SessionProjectionReset(projection) => {
                if !SurfaceSessionProjectionState::accepts_reset(&projection)
                    || !SurfaceOperationProjectionState::accepts_reset(&projection)
                {
                    return;
                }
                self.reset_session_projection();
                self.apply_surface_projection_state(*projection);
            }
            TuiEvent::SavedSessionsUpdated {
                sessions,
                next_offset,
                backfill_complete,
                notice,
            } => {
                self.session_picker_sessions = sessions;
                self.session_picker_next_offset = next_offset;
                self.session_picker_backfill_complete = backfill_complete;
                self.reset_session_selection_to_first_match();
                self.session_picker_phase = SessionPickerPhase::Browsing;
                self.session_picker_error = None;
                self.push_message(ChatMessage::System(notice));
                self.set_status(AppStatus::SessionPicker);
            }
            TuiEvent::SavedSessionActionFailed(message) => {
                self.session_picker_error = Some(message);
                self.session_picker_phase = SessionPickerPhase::Browsing;
                self.set_status(AppStatus::SessionPicker);
            }
            TuiEvent::HistoryLoaded {
                messages,
                plan,
                label,
            } => {
                self.replace_messages(messages);
                if let Some(plan) = plan {
                    self.restore_plan(Some(plan));
                }
                self.push_message(ChatMessage::System(label));
                self.finalized_count = self.messages.len();
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::TurnStarted { .. } => {
                self.suppress_background_main_session_output = false;
                self.enter_running();
            }
            TuiEvent::QueuedSubmissionStarted { id } => {
                self.settle_queued_submission_started(id);
                self.enter_running();
            }
            TuiEvent::BackgroundTaskOutputAttached { .. } => {
                self.suppress_background_main_session_output = false;
                self.panel_mode = PanelMode::Conversation;
            }
            TuiEvent::ReasoningDelta(text) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.finish_assistant_stream();
                let last = self.messages.len().saturating_sub(1);
                if matches!(self.messages.last(), Some(ChatMessage::Reasoning(_))) {
                    self.mutate_message(last, |message| {
                        let ChatMessage::Reasoning(existing) = message else {
                            unreachable!();
                        };
                        existing.push_str(&text);
                    });
                } else {
                    self.push_message(ChatMessage::Reasoning(text));
                }
            }
            TuiEvent::MessageDelta(text) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.handle_message_delta(&text);
            }
            TuiEvent::AssistantResponseCompleted(message, reasoning) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.reconcile_assistant_response(message.as_deref(), reasoning.as_deref());
            }
            TuiEvent::ToolRequested { id, name, target } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name == "subagent" || name == "update_plan" {
                    return;
                }
                if let Some(index) = self.receiving_tool_call_message_index(&id) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            name: existing_name,
                            target: existing_target,
                            status,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_name = name;
                        *existing_target = target;
                        *status = "running".to_string();
                    });
                    return;
                }
                self.finish_assistant_stream();
                self.push_message(ChatMessage::ToolCall {
                    id,
                    name,
                    target,
                    status: "running".to_string(),
                    output: None,
                    diff: None,
                    kind: None,
                    expanded: false,
                });
            }
            TuiEvent::ToolCallProgress {
                id,
                name,
                arguments_bytes,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name
                    .as_deref()
                    .is_some_and(is_panel_owned_tool_progress_name)
                {
                    return;
                }
                let progress_output = Some(format!(
                    "receiving arguments... {}",
                    format_argument_bytes(arguments_bytes)
                ));
                if let Some(index) = self.receiving_tool_call_message_index(&id) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            name: existing_name,
                            status,
                            output,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        if let Some(name) = name {
                            *existing_name = name;
                        }
                        *status = "receiving".to_string();
                        *output = progress_output;
                    });
                } else {
                    self.finish_assistant_stream();
                    self.push_message(ChatMessage::ToolCall {
                        id,
                        name: name.unwrap_or_else(|| "tool".to_string()),
                        target: None,
                        status: "receiving".to_string(),
                        output: progress_output,
                        diff: None,
                        kind: None,
                        expanded: false,
                    });
                }
            }
            TuiEvent::ToolOutputDelta { id, chunk } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == &id)
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall { output, .. } = message else {
                            unreachable!();
                        };
                        output.get_or_insert_with(String::new).push_str(&chunk);
                    });
                }
            }
            TuiEvent::ToolCompleted {
                id,
                name,
                status,
                output,
                diff,
                kind,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name == "update_plan" {
                    // update_plan renders through the pinned plan panel, not
                    // the scrollback; a failed call means that panel is now
                    // showing outdated statuses.
                    if status != "completed" {
                        self.mark_plan_update_failed();
                    }
                    return;
                }
                if name == "subagent" {
                    return;
                }
                let message_index = if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(
                        message,
                        ChatMessage::ToolCall {
                            id: existing_id,
                            status,
                            ..
                        } if existing_id == &id
                            && matches!(status.as_str(), "running" | "receiving")
                    )
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            id: existing_id,
                            name: existing_name,
                            status: existing_status,
                            output: existing_output,
                            diff: existing_diff,
                            kind: existing_kind,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_id = id.clone();
                        *existing_name = name.clone();
                        *existing_status = status.clone();
                        *existing_output = if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        };
                        *existing_diff = diff.clone();
                        *existing_kind = kind.clone();
                    });
                    index
                } else {
                    self.finish_assistant_stream();
                    let index = self.messages.len();
                    self.push_message(ChatMessage::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        target: None,
                        status: status.clone(),
                        output: if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        },
                        diff: diff.clone(),
                        kind: kind.clone(),
                        expanded: false,
                    });
                    index
                };
                if status == "completed" {
                    self.submit_edit_highlight_for_message(message_index);
                }
            }
            TuiEvent::PlanUpdated { explanation, plan } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                // The live plan is shown in the bottom panel during the turn. It is archived
                // inline (and the panel cleared) when the turn completes, so we avoid pushing a
                // message on every update to keep the scrollback clean.
                self.apply_plan_update(explanation, plan);
            }
            TuiEvent::SubagentStarted { id, description } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.finish_assistant_stream();
                self.push_message(ChatMessage::Subagent {
                    id,
                    description,
                    status: "running".to_string(),
                    output: None,
                    error: None,
                    activity: None,
                    activity_tail: Vec::new(),
                    turn: None,
                    usage: None,
                    expanded: false,
                });
            }
            TuiEvent::SubagentCompleted {
                id,
                description,
                status,
                output,
                error,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                let updated = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::Subagent { id: existing_id, .. } if existing_id == &id)
                });

                if let Some(index) = updated {
                    self.mutate_message(index, |message| {
                        let ChatMessage::Subagent {
                            status: existing_status,
                            output: existing_output,
                            error: existing_error,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_status = status;
                        *existing_output = output;
                        *existing_error = error;
                    });
                } else {
                    self.finish_assistant_stream();
                    self.push_message(ChatMessage::Subagent {
                        id,
                        description,
                        status,
                        output,
                        error,
                        activity: None,
                        activity_tail: Vec::new(),
                        turn: None,
                        usage: None,
                        expanded: false,
                    });
                }
            }
            TuiEvent::SubagentProgress {
                id,
                activity,
                turn,
                usage,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::Subagent { id: existing_id, .. } if existing_id == &id)
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::Subagent {
                            activity: existing_activity,
                            activity_tail,
                            turn: existing_turn,
                            usage: existing_usage,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        push_subagent_activity_tail(activity_tail, &activity);
                        *existing_activity = Some(activity);
                        if turn.is_some() {
                            *existing_turn = turn;
                        }
                        if usage.is_some() {
                            *existing_usage = usage;
                        }
                    });
                }
            }
            TuiEvent::WorkflowTasksUpdated { tasks } => self.apply_workflow_tasks_update(tasks),
            TuiEvent::WorkflowTaskUpdated { task } => {
                let mut tasks = self.workflow_panel.tasks.clone();
                if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == task.id) {
                    *existing = task;
                } else {
                    tasks.push(task);
                }
                self.apply_workflow_tasks_update(tasks);
            }
            TuiEvent::WorkflowNotification {
                id,
                prompt,
                status,
                summary,
            } => {
                if self
                    .push_pending_workflow_notification(PendingWorkflowNotification { id, prompt })
                {
                    self.finish_assistant_stream();
                    self.push_message(ChatMessage::System(format!("Workflow {status}. {summary}")));
                }
            }
            TuiEvent::ApprovalNeeded {
                key,
                tool,
                target,
                preview,
            } => {
                self.close_transcript_search();
                self.set_status(AppStatus::WaitingApproval);
                let options = ApprovalDialog::options_for(&tool, target.as_deref());
                self.approval_dialog = Some(ApprovalDialog {
                    id: key.request_id.clone(),
                    interaction: Some(key),
                    tool,
                    target,
                    permission_kind: None,
                    background_task_id: None,
                    selected: 0,
                    options,
                    diff: preview,
                });
            }
            TuiEvent::PermissionApprovalNeeded {
                key,
                tool,
                target,
                preview,
                permission_kind,
            } => {
                self.close_transcript_search();
                self.set_status(AppStatus::WaitingApproval);
                let options = ApprovalDialog::options_for(&tool, target.as_deref());
                self.approval_dialog = Some(ApprovalDialog {
                    id: key.request_id.clone(),
                    interaction: Some(key),
                    tool,
                    target,
                    permission_kind: Some(permission_kind),
                    background_task_id: None,
                    selected: 0,
                    options,
                    diff: preview,
                });
            }
            TuiEvent::UserInputRequested {
                key,
                question,
                choices,
            } => {
                self.set_status(AppStatus::WaitingUserInput);
                self.pending_input = Some(PendingTuiInput::UserInput(key));
                self.finish_assistant_stream();
                let mut message = question;
                if !choices.is_empty() {
                    message.push_str("\nChoices: ");
                    message.push_str(&choices.join(", "));
                }
                self.push_message(ChatMessage::System(message));
            }
            TuiEvent::McpElicitationRequested {
                key,
                server_name,
                mode,
                message,
                url,
                requested_schema_json,
            } => {
                self.set_status(AppStatus::WaitingUserInput);
                self.pending_input = Some(PendingTuiInput::McpElicitation(key));
                self.finish_assistant_stream();
                let mut lines = vec![format!("MCP {server_name} requests input: {message}")];
                match mode {
                    RuntimeMcpElicitationMode::Form => {
                        lines.push("Mode: form".to_string());
                        if let Some(schema) = requested_schema_json {
                            lines.push(format!("Schema: {schema}"));
                        }
                    }
                    RuntimeMcpElicitationMode::Url => {
                        lines.push("Mode: url".to_string());
                        if let Some(url) = url {
                            lines.push(format!("URL: {url}"));
                        }
                    }
                }
                self.push_message(ChatMessage::System(lines.join("\n")));
            }
            TuiEvent::SubmissionRejected {
                queued_id,
                prompt: _,
                message,
            } => {
                if queued_id.is_some()
                    && !queued_id.is_some_and(|id| self.queued_submission_matches_id(id))
                {
                    self.push_message(ChatMessage::Error(message));
                    return;
                }
                self.remove_after_last_user();
                self.mention_bindings.clear();
                self.atomic_skill_tokens.clear();
                self.clear_receiving_tool_progress();
                self.push_message(ChatMessage::Error(message));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::OperationRejected(message) => {
                self.reset_assistant_stream();
                self.clear_receiving_tool_progress();
                self.push_message(ChatMessage::Error(message));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::Error(msg) => {
                self.finish_assistant_stream();
                self.clear_receiving_tool_progress();
                self.push_message(ChatMessage::Error(msg));
            }
            TuiEvent::Notice(msg) => {
                self.finish_assistant_stream();
                self.push_message(ChatMessage::System(msg));
            }
            TuiEvent::MentionSearchDirty { .. }
            | TuiEvent::MentionCatalogDirty { .. }
            | TuiEvent::MentionRuntimeReady(_) => {}
            TuiEvent::CompactionStarted => {
                self.set_status(AppStatus::Compacting);
            }
            TuiEvent::SettingsUpdated {
                model,
                reasoning_effort,
                approval_mode,
            } => {
                let previous_mode = self.approval_mode;
                if approval_mode == ApprovalMode::Plan && previous_mode != ApprovalMode::Plan {
                    self.pre_plan_approval_mode = Some(previous_mode);
                } else if approval_mode != ApprovalMode::Plan && previous_mode == ApprovalMode::Plan
                {
                    self.pre_plan_approval_mode = None;
                    self.plan_approval_dialog = None;
                }
                self.model_name = model;
                self.reasoning_effort = reasoning_effort;
                self.approval_mode = approval_mode;
                self.push_message(ChatMessage::System(format!(
                    "Runtime settings updated: model {}, reasoning effort {}, approval mode {}.",
                    self.model_name,
                    self.reasoning_effort.as_str(),
                    self.approval_mode.as_str()
                )));
            }
            TuiEvent::PlanImplementationStarted { prompt } => {
                self.record_prompt(prompt.clone());
                self.push_message(ChatMessage::User(prompt));
                self.enter_running();
                self.scroll_to_bottom();
            }
            TuiEvent::SessionCompleted { status } => {
                let was_backgrounded = self.suppress_background_main_session_output;
                self.suppress_background_main_session_output = false;
                self.approval_dialog = None;
                self.pending_input = None;
                self.clear_receiving_tool_progress();
                self.flush_proposed_plan_parser();
                self.finish_assistant_stream();
                self.promote_trailing_reasoning();
                self.archive_current_plan();
                let proposed_plan = (status == "success"
                    && self.approval_mode == ApprovalMode::Plan
                    && !was_backgrounded)
                    .then(|| self.current_turn_proposed_plan())
                    .flatten();
                if was_backgrounded {
                    self.push_message(ChatMessage::System(format!(
                        "Background session completed: {status}"
                    )));
                }
                self.finalize_turn();
                self.set_status(AppStatus::Idle);
                if let Some(plan) = proposed_plan {
                    self.plan_approval_dialog = Some(PlanApprovalDialog { plan, selected: 0 });
                    self.suspend_queued_follow_up_autosend();
                }
                self.last_completed_at = Some(Instant::now());
                self.scroll_to_bottom();
            }
            TuiEvent::Compacted {
                before_messages,
                after_messages,
                reason,
                strategy,
                collapsed_messages,
                status_text,
            } => {
                self.finish_assistant_stream();
                self.push_message(ChatMessage::System(format_compaction_notice(
                    &reason,
                    &strategy,
                    before_messages,
                    after_messages,
                    collapsed_messages,
                    &status_text,
                )));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::GoalStatus(goal) => {
                let mut should_keep_running = false;
                match goal {
                    Some(goal) => {
                        should_keep_running =
                            self.status == AppStatus::Running && goal.status.should_continue();
                        let notice = format_goal_notice(&goal);
                        self.push_goal_notice(notice);
                    }
                    None => {
                        self.finish_assistant_stream();
                        self.push_message(ChatMessage::System(
                            "No goal is currently set.".to_string(),
                        ));
                    }
                }
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            TuiEvent::Backtracked { prompt } => {
                self.remove_after_last_user();
                self.push_message(ChatMessage::System(format!(
                    "Backtracked to previous prompt: {}",
                    prompt.trim()
                )));
                self.set_status(AppStatus::Idle);
            }
        }
    }

    fn promote_trailing_reasoning(&mut self) {
        let index = self.messages.len().saturating_sub(1);
        if let Some(ChatMessage::Reasoning(text)) = self.messages.get(index) {
            let text = text.clone();
            self.replace_message(index, ChatMessage::Assistant(text));
        }
    }

    fn reconcile_assistant_response(&mut self, message: Option<&str>, reasoning: Option<&str>) {
        let last_user = self
            .messages
            .iter()
            .rposition(|item| matches!(item, ChatMessage::User(_)));
        if let Some(last_user) = last_user {
            let mut index = 0;
            self.retain_messages(|item| {
                let keep = index <= last_user
                    || !matches!(
                        item,
                        ChatMessage::Reasoning(_)
                            | ChatMessage::Assistant(_)
                            | ChatMessage::AssistantChunk { .. }
                            | ChatMessage::ProposedPlan(_)
                    );
                index += 1;
                keep
            });
        }
        self.proposed_plan_parser = ProposedPlanStreamParser::default();
        // Streaming markdown may still hold an unfinished partial line from the
        // content being replaced; drop it so the completed response renders alone.
        self.reset_assistant_stream();
        if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
            self.push_message(ChatMessage::Reasoning(reasoning.to_string()));
        }
        if let Some(message) = message.filter(|text| !text.is_empty()) {
            self.handle_message_delta(message);
        }
    }

    fn handle_message_delta(&mut self, text: &str) {
        for segment in self.proposed_plan_parser.push(text) {
            self.push_proposed_plan_segment(segment);
        }
    }

    fn flush_proposed_plan_parser(&mut self) {
        for segment in self.proposed_plan_parser.finish() {
            self.push_proposed_plan_segment(segment);
        }
    }

    fn push_proposed_plan_segment(&mut self, segment: ProposedPlanSegment) {
        match segment {
            ProposedPlanSegment::Agent(text) => {
                let actions = self.assistant_stream.push(&text);
                self.apply_streaming_markdown_actions(actions);
            }
            ProposedPlanSegment::Plan(text) => {
                self.finish_assistant_stream();
                self.push_proposed_plan_delta(text);
            }
        }
    }

    fn apply_streaming_markdown_actions(&mut self, actions: Vec<StreamingMarkdownAction>) {
        for action in actions {
            match action {
                StreamingMarkdownAction::UpdateTail(text) => {
                    if let Some(index) = self.assistant_stream_tail {
                        self.mutate_message(index, |message| {
                            let ChatMessage::Assistant(existing) = message else {
                                unreachable!();
                            };
                            *existing = text;
                        });
                    } else {
                        let index = self.messages.len();
                        self.push_message(ChatMessage::Assistant(text));
                        self.assistant_stream_tail = Some(index);
                    }
                }
                StreamingMarkdownAction::FreezeTail {
                    text,
                    trailing_blank,
                } => {
                    if let Some(index) = self.assistant_stream_tail {
                        self.replace_message(
                            index,
                            ChatMessage::AssistantChunk {
                                text,
                                trailing_blank,
                            },
                        );
                    } else {
                        self.push_message(ChatMessage::AssistantChunk {
                            text,
                            trailing_blank,
                        });
                    }
                }
                StreamingMarkdownAction::AppendFrozen {
                    text,
                    trailing_blank,
                } => self.push_message(ChatMessage::AssistantChunk {
                    text,
                    trailing_blank,
                }),
                StreamingMarkdownAction::ClearTail => {
                    self.assistant_stream_tail = None;
                }
                StreamingMarkdownAction::FinishTail(suffix) => {
                    if let Some(index) = self.assistant_stream_tail {
                        if !suffix.is_empty() {
                            self.mutate_message(index, |message| {
                                let ChatMessage::Assistant(existing) = message else {
                                    unreachable!();
                                };
                                existing.push_str(&suffix);
                            });
                        }
                    } else if !suffix.is_empty() {
                        self.push_message(ChatMessage::Assistant(suffix));
                    }
                    self.assistant_stream_tail = None;
                }
            }
        }
    }

    fn finish_assistant_stream(&mut self) {
        let actions = self.assistant_stream.finish();
        self.apply_streaming_markdown_actions(actions);
        self.assistant_stream = StreamingMarkdownAssembler::default();
        self.assistant_stream_tail = None;
        let Some(index) = self.messages.len().checked_sub(1) else {
            return;
        };
        let needs_separator = matches!(
            self.messages.get(index),
            Some(ChatMessage::AssistantChunk {
                trailing_blank: false,
                ..
            })
        );
        if needs_separator {
            self.mutate_message(index, |message| {
                let ChatMessage::AssistantChunk { trailing_blank, .. } = message else {
                    unreachable!();
                };
                *trailing_blank = true;
            });
        }
    }

    fn reset_assistant_stream(&mut self) {
        self.assistant_stream = StreamingMarkdownAssembler::default();
        self.assistant_stream_tail = None;
    }

    fn push_proposed_plan_delta(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let last = self.messages.len().saturating_sub(1);
        if matches!(self.messages.last(), Some(ChatMessage::ProposedPlan(_))) {
            self.mutate_message(last, |message| {
                let ChatMessage::ProposedPlan(existing) = message else {
                    unreachable!();
                };
                existing.push_str(&text);
            });
        } else {
            self.push_message(ChatMessage::ProposedPlan(text));
        }
    }

    fn current_turn_proposed_plan(&self) -> Option<String> {
        self.messages
            .get(self.finalized_count.min(self.messages.len())..)
            .into_iter()
            .flatten()
            .rev()
            .find_map(|message| match message {
                ChatMessage::ProposedPlan(plan) if !plan.trim().is_empty() => Some(plan.clone()),
                _ => None,
            })
    }

    /// Move the live plan out of the bottom panel and into the scrollback as an archived
    /// checklist when a turn ends, so the panel stops occluding content once work is done.
    fn archive_current_plan(&mut self) {
        if let Some((explanation, plan)) = self.take_plan_for_archive() {
            self.push_message(ChatMessage::PlanUpdate { explanation, plan });
        }
    }

    /// Freeze the current turn: everything in `messages` becomes the immutable,
    /// finalized prefix. Called once a turn ends, after trailing reasoning is promoted
    /// and the live plan is archived, so the frozen transcript is in its final shape.
    fn finalize_turn(&mut self) {
        self.finalized_count = self.messages.len();
    }

    fn clear_receiving_tool_progress(&mut self) {
        let original_finalized_count = self.finalized_count;
        let has_receiving_progress = self.messages[original_finalized_count.min(self.messages.len())..]
            .iter()
            .any(|message| {
                matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            });
        if !has_receiving_progress {
            return;
        }
        let mut index = 0;
        self.retain_messages(|message| {
            let remove = index >= original_finalized_count
                && matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving");
            index += 1;
            !remove
        });
    }

    /// Whether the message at `index` will never change again, so it is safe to flush
    /// into the append-only scrollback.
    ///
    /// - A finalized message (`index < finalized_count`) is frozen by definition.
    /// - A `ToolCall`/`Subagent` is settled once it leaves the `running` status; its
    ///   output/diff are then complete.
    /// - A `Reasoning`/`Assistant`/`ProposedPlan` block grows via streaming deltas only
    ///   while it is the last message, so it is settled once a newer message follows it,
    ///   or once the turn ends (`turn_ended`).
    /// - Everything else (`User`/`Error`/`System`/`PlanUpdate`) is immutable on arrival.
    fn message_is_settled(&self, index: usize, turn_ended: bool) -> bool {
        if index < self.finalized_count {
            return true;
        }
        let is_last = index + 1 == self.messages.len();
        match &self.messages[index] {
            ChatMessage::ToolCall { status, .. } | ChatMessage::Subagent { status, .. } => {
                !matches!(status.as_str(), "running" | "receiving")
            }
            ChatMessage::Reasoning(_)
            | ChatMessage::Assistant(_)
            | ChatMessage::ProposedPlan(_) => turn_ended || !is_last,
            ChatMessage::AssistantChunk { .. }
            | ChatMessage::User(_)
            | ChatMessage::Error(_)
            | ChatMessage::System(_)
            | ChatMessage::PlanUpdate { .. } => true,
        }
    }

    /// The new value `flushed_count` may advance to: the end of the longest run of
    /// settled messages starting at the current `flushed_count`. Scrollback is
    /// append-only, so a single unsettled message (e.g. a still-running tool call)
    /// blocks everything after it from flushing, even if those later messages are
    /// themselves settled — flushing them now would print them out of order.
    pub fn flushable_prefix_end(&self, turn_ended: bool) -> usize {
        let mut end = self.flushed_count;
        while end < self.messages.len() && self.message_is_settled(end, turn_ended) {
            end += 1;
        }
        end
    }

    pub fn remove_after_last_user(&mut self) {
        if let Some(index) = self
            .messages
            .iter()
            .rposition(|message| matches!(message, ChatMessage::User(_)))
        {
            self.truncate_messages(index);
        }
    }
}

fn push_subagent_activity_tail(tail: &mut Vec<String>, activity: &str) {
    if tail.last().is_some_and(|last| last == activity) {
        return;
    }
    tail.push(activity.to_string());
    if tail.len() > SUBAGENT_ACTIVITY_TAIL_LIMIT {
        tail.drain(0..tail.len() - SUBAGENT_ACTIVITY_TAIL_LIMIT);
    }
}

fn format_argument_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn is_panel_owned_tool_progress_name(name: &str) -> bool {
    matches!(name, "subagent" | "update_plan")
}

fn format_compaction_notice(
    reason: &str,
    strategy: &str,
    before_messages: usize,
    after_messages: usize,
    collapsed_messages: usize,
    status_text: &str,
) -> String {
    let label = compaction_notice_label(reason, status_text);
    let detail = if collapsed_messages > 0 && !strategy.trim().is_empty() {
        format!(" (collapsed {collapsed_messages}, {strategy})")
    } else if collapsed_messages > 0 {
        format!(" (collapsed {collapsed_messages})")
    } else if !strategy.trim().is_empty() {
        format!(" ({strategy})")
    } else {
        String::new()
    };
    format!(
        "Compacted conversation context {label}: {before_messages} -> {after_messages} messages{detail}."
    )
}

fn compaction_notice_label(reason: &str, status_text: &str) -> String {
    let status = status_text.trim();
    if let Some(rest) = status.strip_prefix("compacted context ") {
        return rest.to_string();
    }
    match reason {
        "prompt_too_long_recovery" => "after prompt-too-long".to_string(),
        "exceeded_context_limit" => "at token limit".to_string(),
        "approaching_context_limit" => "near token limit".to_string(),
        "manual" => "manually".to_string(),
        value if !value.trim().is_empty() => value.replace('_', " "),
        _ => "completed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::plan_types::PlanStatus;
    use orca_core::task_types::{TaskStatus, TaskType};

    fn inserted_source_line<'a>(
        lines: &'a [ratatui::text::Line<'static>],
        source: &str,
    ) -> &'a ratatui::text::Line<'static> {
        lines
            .iter()
            .find(|line| {
                line.to_string().contains(source)
                    && line
                        .spans
                        .first()
                        .is_some_and(|span| span.content.ends_with("+ "))
            })
            .unwrap_or_else(|| panic!("inserted source line containing {source:?}"))
    }

    fn normalized_source_spans(
        spans: &[ratatui::text::Span<'_>],
    ) -> crate::syntax_highlight::StyledSourceLine {
        let mut output: crate::syntax_highlight::StyledSourceLine = Vec::new();
        for span in spans {
            let mut style = span.style;
            style.bg = None;
            if let Some(previous) = output.last_mut()
                && previous.style == style
            {
                previous.content.to_mut().push_str(span.content.as_ref());
                continue;
            }
            output.push(ratatui::text::Span::styled(span.content.to_string(), style));
        }
        output
    }

    fn state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    fn prepare_transcript_cache(state: &mut AppState, width: usize) {
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let messages = &state.messages;
        let revisions = &state.message_revisions;
        state.transcript_render_cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, width, 0, false),
            |_, message, theme, width, tick, force_expand| {
                crate::ui::build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
    }

    fn queued(text: &str) -> crate::queued_input::QueuedUserMessage {
        crate::queued_input::QueuedUserMessage::from_composer(
            text.to_string(),
            Vec::new(),
            orca_runtime::mentions::MentionBindings::default(),
        )
        .unwrap()
    }

    #[test]
    fn conversation_replacement_resets_all_queued_follow_up_state() {
        for clear in [false, true] {
            let mut state = state();
            state.enqueue_user_message(queued("in flight")).unwrap();
            state.set_status(AppStatus::Idle);
            state
                .begin_next_queued_message()
                .expect("seed in-flight queued submission");
            state.enqueue_user_message(queued("queued")).unwrap();
            state.suspend_queued_follow_up_autosend();
            state.report_queued_input_error("full".to_string());

            if clear {
                state.clear_messages();
            } else {
                state.replace_messages([ChatMessage::System("replacement".to_string())]);
            }

            assert!(state.queued_pending_visible_text().is_empty());
            assert!(!state.queued_submission_in_flight());
            assert!(state.queued_autosend_enabled());
            assert!(state.queued_input_error().is_none());
            assert!(!state.queued_follow_up_pending_or_in_flight());

            state.enqueue_user_message(queued("after reset")).unwrap();
            state.set_status(AppStatus::Idle);
            state.begin_next_queued_message().unwrap();
            assert_eq!(state.queued_in_flight_id(), Some(1));
        }
    }

    #[test]
    fn opening_search_preserves_scroll_and_refresh_selects_viewport_match() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant(
            "first hit\nsecond\nthird hit".to_string(),
        ));
        prepare_transcript_cache(&mut state, 20);
        state.scroll_offset = 1;
        state.viewport_base_row = 1;
        state.open_transcript_search();
        state.replace_transcript_search_query("hit");
        state.refresh_transcript_search();

        assert!(state.transcript_search.open);
        assert_eq!(state.scroll_offset, 1);
        assert_eq!(
            state
                .transcript_search
                .active_match()
                .map(|found| found.start.row),
            Some(2)
        );
    }

    #[test]
    fn explicit_search_jump_disables_follow_and_reveals_match() {
        let mut state = state();
        for index in 0..30 {
            state.push_message(ChatMessage::System(format!("line {index} target")));
        }
        prepare_transcript_cache(&mut state, 80);
        state.visible_height = 5;
        state.scroll_offset = 20;
        state.auto_scroll = true;
        state.open_transcript_search();
        state.replace_transcript_search_query("target");
        state.refresh_transcript_search();

        state.search_next();

        assert!(!state.auto_scroll);
        let active = state.transcript_search.active_match().unwrap();
        assert!(active.start.row >= state.scroll_offset);
        assert!(active.start.row < state.scroll_offset + state.visible_height);
    }

    #[test]
    fn clear_resets_search_but_truncate_reconciles_lazily() {
        let mut state = state();
        state.open_transcript_search();
        state.replace_transcript_search_query("x");
        state.push_message(ChatMessage::System("x".to_string()));
        prepare_transcript_cache(&mut state, 40);
        state.refresh_transcript_search();
        assert_eq!(state.transcript_search.match_count(), 1);

        state.truncate_messages(0);
        state.refresh_transcript_search();
        assert_eq!(state.transcript_search.match_count(), 0);
        assert_eq!(state.transcript_search.query(), "x");

        state.clear_messages();
        assert!(!state.transcript_search.open);
        assert_eq!(state.transcript_search.query(), "");
    }

    #[test]
    fn append_and_retain_preserve_active_revision_identity() {
        let mut state = state();
        state.push_message(ChatMessage::System("remove".to_string()));
        state.push_message(ChatMessage::System("target".to_string()));
        prepare_transcript_cache(&mut state, 40);
        state.open_transcript_search();
        state.replace_transcript_search_query("target");
        state.refresh_transcript_search();
        let identity = state
            .transcript_search
            .active_match()
            .unwrap()
            .line_identity;

        state.push_message(ChatMessage::System("later target".to_string()));
        prepare_transcript_cache(&mut state, 40);
        state.refresh_transcript_search();
        assert_eq!(
            state
                .transcript_search
                .active_match()
                .unwrap()
                .line_identity,
            identity
        );

        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
        );
        prepare_transcript_cache(&mut state, 40);
        state.refresh_transcript_search();
        assert_eq!(
            state
                .transcript_search
                .active_match()
                .unwrap()
                .line_identity,
            identity
        );
    }

    #[test]
    fn one_append_rebuilds_one_message_then_rescans_without_render_rebuilds() {
        let mut state = state();
        for index in 0..1_000 {
            state.push_message(ChatMessage::System(format!("item {index} needle")));
        }
        prepare_transcript_cache(&mut state, 80);
        state.open_transcript_search();
        state.replace_transcript_search_query("needle");
        state.refresh_transcript_search();
        let scans = state.transcript_search.scan_count_for_test();

        state.push_message(ChatMessage::System("last needle".to_string()));
        prepare_transcript_cache(&mut state, 80);
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
        let render_generation = state.transcript_render_cache.content_generation();
        state.refresh_transcript_search();
        assert_eq!(state.transcript_search.scan_count_for_test(), scans + 1);
        assert_eq!(
            state.transcript_render_cache.content_generation(),
            render_generation
        );
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
    }

    #[test]
    fn removal_chooses_nearest_following_match_and_open_does_not_disable_follow() {
        let mut state = state();
        for text in ["target one", "middle", "target two"] {
            state.push_message(ChatMessage::System(text.to_string()));
        }
        prepare_transcript_cache(&mut state, 40);
        state.auto_scroll = true;
        state.open_transcript_search();
        assert!(state.auto_scroll);
        state.replace_transcript_search_query("target");
        state.refresh_transcript_search();
        let first_revision = state
            .transcript_search
            .active_match()
            .unwrap()
            .line_identity;

        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "target one"),
        );
        prepare_transcript_cache(&mut state, 40);
        state.refresh_transcript_search();
        assert_ne!(
            state
                .transcript_search
                .active_match()
                .unwrap()
                .line_identity,
            first_revision
        );
        assert_eq!(state.transcript_search.match_count(), 1);
    }

    #[test]
    fn approval_closes_search_but_preserves_query() {
        let mut state = state();
        state.open_transcript_search();
        state.replace_transcript_search_query("target");
        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval"),
            tool: "bash".to_string(),
            target: None,
            preview: None,
        });

        assert!(!state.transcript_search.open);
        assert_eq!(state.transcript_search.query(), "target");
    }

    #[test]
    fn fresh_app_state_has_default_syntax_highlight_state() {
        let state = state();

        assert!(state.workspace_git.is_none());
        assert!(state.syntax_workspace_root_for_test().is_none());
        assert_eq!(
            state.syntax_theme_for_test(),
            crate::syntax_highlight::SyntaxTheme::OneHalfDark
        );
        assert_eq!(
            state.syntax_color_level_for_test(),
            crate::terminal_capabilities::TerminalColorLevel::TrueColor
        );
        assert!(!state.edit_highlight_runtime_started_for_test());
        assert!(state.edit_highlights.applied().is_empty());
    }

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(
            orca_core::cancel::OperationIdAllocator::new().allocate(),
            id,
            kind,
        )
    }

    fn dummy_selection() -> crate::selection::TranscriptSelection {
        let pos = crate::selection::SelectionPos { row: 0, col: 0 };
        let end = crate::selection::SelectionPos { row: 1, col: 3 };
        crate::selection::TranscriptSelection {
            anchor: pos,
            head: end,
            dragging: false,
            granularity: crate::selection::SelectionGranularity::Cell,
            origin: (pos, end),
        }
    }

    #[test]
    fn transcript_mutations_invalidate_the_selection_only_when_rows_can_shift() {
        let mut state = state();
        state.push_message(ChatMessage::System("one".to_string()));
        state.push_message(ChatMessage::System("two".to_string()));
        state.push_message(ChatMessage::System("three".to_string()));

        // Appending and rewriting the TAIL keep the selection: earlier rows
        // cannot move.
        state.selection = Some(dummy_selection());
        state.push_message(ChatMessage::System("four".to_string()));
        assert!(state.selection.is_some());
        state.touch_message(state.messages.len() - 1);
        assert!(state.selection.is_some());

        // Rewriting a non-tail message can change its height: cleared.
        state.touch_message(1);
        assert_eq!(state.selection, None);

        // Removing messages shifts rows: cleared.
        state.selection = Some(dummy_selection());
        state.truncate_messages(3);
        assert_eq!(state.selection, None);

        state.selection = Some(dummy_selection());
        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "two"),
        );
        assert_eq!(state.selection, None);

        // A retain that keeps everything moves nothing: selection survives.
        state.selection = Some(dummy_selection());
        state.retain_messages(|_| true);
        assert!(state.selection.is_some());

        state.selection = Some(dummy_selection());
        state.clear_messages();
        assert_eq!(state.selection, None);
    }

    #[test]
    fn history_loaded_replaces_legacy_prefix_and_freezes_snapshot() {
        let mut state = state();
        state.push_message(ChatMessage::User("legacy".to_string()));

        state.update(TuiEvent::HistoryLoaded {
            messages: vec![
                ChatMessage::User("restored".to_string()),
                ChatMessage::Assistant("answer".to_string()),
            ],
            plan: Some((
                Some("resume plan".to_string()),
                vec![PlanItem {
                    step: "continue".to_string(),
                    status: PlanStatus::InProgress,
                }],
            )),
            label: "Resumed saved conversation.".to_string(),
        });

        assert!(matches!(
            state.messages.as_slice(),
            [
                ChatMessage::User(prompt),
                ChatMessage::Assistant(answer),
                ChatMessage::System(label),
            ] if prompt == "restored"
                && answer == "answer"
                && label == "Resumed saved conversation."
        ));
        assert_eq!(state.finalized_count, state.messages.len());
        // `flushed_count` must stay 0 in the fullscreen TUI: it counts messages
        // omitted from the live renderer, so setting it to the message count made
        // `live_start` skip the whole transcript and blanked the pane on switch.
        assert_eq!(state.flushed_count, 0);
        assert_eq!(
            state.current_plan().unwrap().0.as_deref(),
            Some("resume plan")
        );
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn new_session_started_resets_conversation_state_and_preserves_runtime_settings() {
        let mut state = state();
        state.push_message(ChatMessage::User("old prompt".to_string()));
        state.replace_plan_for_test(Some((
            Some("old plan".to_string()),
            vec![PlanItem {
                step: "old step".to_string(),
                status: PlanStatus::InProgress,
            }],
        )));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("old-session".to_string()),
                title: "Old session".to_string(),
                usage_revision: 1,
                usage: UsageTotals {
                    input_tokens: 42,
                    ..UsageTotals::default()
                },
                context_revision: 1,
                context_used_tokens: 21,
                context_limit_tokens: 100,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
            },
        )));
        state.approval_allowlist.insert("bash".to_string());
        state.model_name = "deepseek-v4-pro".to_string();
        state.reasoning_effort = orca_core::config::ReasoningEffort::High;
        state.approval_mode = ApprovalMode::FullAuto;
        state.history_cursor = Some(0);
        state.draft_before_history = Some("old draft".to_string());
        state.last_ctrl_c = Some(Instant::now());
        state.pending_clipboard_copy = Some("old selection".to_string());
        state.copy_notice = Some(CopyNotice {
            chars: 13,
            at: Instant::now(),
            local_only: false,
        });
        state.last_left_click = Some((Instant::now(), 1, 1, 1));
        state.composer_mouse_selecting = true;
        state.enter_running();

        state.update(TuiEvent::SessionProjectionReset(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(2),
                session_id: Some("019f8a00-0000-7000-8000-000000000123".to_string()),
                title: "New conversation".to_string(),
                usage_revision: 1,
                usage: UsageTotals::default(),
                context_revision: 1,
                context_used_tokens: 0,
                context_limit_tokens: 0,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
            },
        )));
        state.update(TuiEvent::NewSessionStarted);

        assert!(state.messages.is_empty());
        assert!(state.current_plan().is_none());
        assert_eq!(state.usage(), &UsageTotals::default());
        assert_eq!(state.context_used_tokens(), 0);
        assert_eq!(state.context_limit_tokens(), 0);
        assert!(state.approval_allowlist.is_empty());
        assert_eq!(state.status, AppStatus::Idle);
        assert_eq!(state.model_name, "deepseek-v4-pro");
        assert_eq!(
            state.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        assert_eq!(state.approval_mode, ApprovalMode::FullAuto);
        assert!(state.history_cursor.is_none());
        assert!(state.draft_before_history.is_none());
        assert!(state.last_ctrl_c.is_none());
        assert!(state.pending_clipboard_copy.is_none());
        assert!(state.copy_notice.is_none());
        assert!(state.last_left_click.is_none());
        assert!(!state.composer_mouse_selecting);
    }

    #[test]
    fn surface_session_projection_updates_current_identity() {
        let mut state = state();

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("session-1".to_string()),
                title: "Auth investigation".to_string(),
                usage_revision: 1,
                usage: UsageTotals::default(),
                context_revision: 1,
                context_used_tokens: 0,
                context_limit_tokens: 0,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
            },
        )));

        assert_eq!(state.current_session_id(), Some("session-1"));
        assert_eq!(state.current_session_title(), Some("Auth investigation"));
    }

    #[test]
    fn nth_final_assistant_response_ignores_streaming_chunks() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("older".to_string()));
        state.push_message(ChatMessage::AssistantChunk {
            text: "unfinished".to_string(),
            trailing_blank: false,
        });
        state.push_message(ChatMessage::Assistant("latest".to_string()));

        assert_eq!(state.nth_final_assistant_response(1), Some("latest"));
        assert_eq!(state.nth_final_assistant_response(2), Some("older"));
        assert_eq!(state.nth_final_assistant_response(0), None);
        assert_eq!(state.nth_final_assistant_response(3), None);
    }

    fn session(id: &str, title: &str) -> SessionSummary {
        use chrono::Utc;
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: "/tmp".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: std::env::temp_dir(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    fn workflow_task_summary(id: &str, name: &str) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: name.to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(name.to_string()),
            workflow_run_id: Some(format!("run-{id}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }
    }

    #[test]
    fn workflow_notification_action_carries_notification_boundary() {
        let expected = PendingWorkflowNotification {
            id: "notice-1".to_string(),
            prompt: "continue the workflow".to_string(),
        };
        let action = UserAction::SubmitWorkflowNotification(expected.clone());

        match action {
            UserAction::SubmitWorkflowNotification(actual) => {
                assert_eq!(actual, expected);
            }
            _ => unreachable!("constructed the workflow notification variant"),
        }
    }

    #[test]
    fn session_search_filters_by_title_and_keeps_selection_valid() {
        let mut state = state();
        state.session_picker_sessions = vec![
            session("a", "fix the failing auth test"),
            session("b", "add JWT auth middleware"),
            session("c", "refactor parser entrypoint"),
        ];
        state.session_picker_selected = 0;

        // No query → all match.
        assert_eq!(state.filtered_session_indices(), vec![0, 1, 2]);

        // Typing "auth" keeps only the two auth sessions and snaps selection
        // to the first match.
        for ch in "auth".chars() {
            state.session_query_push(ch);
        }
        assert_eq!(state.filtered_session_indices(), vec![0, 1]);
        assert_eq!(state.session_picker_selected, 0);

        // Down moves within the filtered set, not the raw list.
        state.select_next_session();
        assert_eq!(state.session_picker_selected, 1);
        state.select_next_session();
        assert_eq!(state.session_picker_selected, 1); // clamped to last match

        // Backspace widens the filter again.
        state.session_query_pop();
        assert_eq!(state.session_picker_query, "aut");
        assert_eq!(state.filtered_session_indices(), vec![0, 1]);
    }

    #[test]
    fn replacing_messages_resets_tracking_after_same_length_replacement() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("old session".to_string()));
        let old_revision = state.message_revisions[0];

        state.replace_messages([ChatMessage::Assistant("new session".to_string())]);

        assert_ne!(state.message_revisions[0], old_revision);
        assert_eq!(state.transcript_render_cache.len(), state.messages.len());
    }

    #[test]
    fn retaining_messages_rebases_watermarks_and_cache_entries() {
        use std::cell::RefCell;

        let mut state = state();
        state.push_message(ChatMessage::User("keep before".to_string()));
        state.push_message(ChatMessage::System("remove before".to_string()));
        state.push_message(ChatMessage::Assistant("keep after".to_string()));
        state.finalized_count = 3;
        state.flushed_count = 2;
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false),
            |_, message, _, _, _, _| vec![ratatui::text::Line::from(format!("{message:?}"))],
        );
        assert_eq!(state.transcript_render_cache.populated_len(), 3);

        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "remove before"),
        );

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.message_revisions.len(), 2);
        assert_eq!(state.finalized_count, 2);
        assert_eq!(state.flushed_count, 1);
        assert_eq!(state.transcript_render_cache.len(), 2);
        assert_eq!(state.transcript_render_cache.populated_len(), 2);

        state.touch_message(1);
        let built_indices = RefCell::new(Vec::new());
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false),
            |index, message, _, _, _, _| {
                built_indices.borrow_mut().push(index);
                vec![ratatui::text::Line::from(format!("{message:?}"))]
            },
        );

        assert_eq!(*built_indices.borrow(), vec![1]);
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
    }

    #[test]
    fn approval_options_have_numeric_primary_keys_and_legacy_shortcuts() {
        assert_eq!(ApprovalOption::Once.key(), '1');
        assert_eq!(ApprovalOption::AlwaysTarget.key(), '2');
        assert_eq!(ApprovalOption::AlwaysTool.key(), '3');
        assert_eq!(ApprovalOption::Deny.key(), '4');

        assert!(ApprovalOption::Once.matches_key('1'));
        assert!(ApprovalOption::Once.matches_key('y'));
        assert!(ApprovalOption::AlwaysTarget.matches_key('2'));
        assert!(ApprovalOption::AlwaysTarget.matches_key('A'));
        assert!(ApprovalOption::AlwaysTool.matches_key('3'));
        assert!(ApprovalOption::AlwaysTool.matches_key('a'));
        assert!(ApprovalOption::Deny.matches_key('4'));
        assert!(ApprovalOption::Deny.matches_key('n'));

        assert!(!ApprovalOption::AlwaysTarget.matches_key('a'));
        assert!(!ApprovalOption::AlwaysTool.matches_key('A'));
    }

    #[test]
    fn approval_dialog_resolves_numeric_and_legacy_keys_by_visible_options() {
        let dialog = ApprovalDialog {
            id: "approval-1".to_string(),
            interaction: None,
            tool: "edit".to_string(),
            target: Some("src/main.rs".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: ApprovalDialog::options_for("edit", Some("src/main.rs")),
            diff: None,
        };

        assert_eq!(dialog.option_for_key('1'), Some(ApprovalOption::Once));
        assert_eq!(
            dialog.option_for_key('2'),
            Some(ApprovalOption::AlwaysTarget)
        );
        assert_eq!(dialog.option_for_key('3'), Some(ApprovalOption::AlwaysTool));
        assert_eq!(dialog.option_for_key('4'), Some(ApprovalOption::Deny));
        assert_eq!(dialog.option_for_key('y'), Some(ApprovalOption::Once));
        assert_eq!(
            dialog.option_for_key('A'),
            Some(ApprovalOption::AlwaysTarget)
        );
        assert_eq!(dialog.option_for_key('a'), Some(ApprovalOption::AlwaysTool));
        assert_eq!(dialog.option_for_key('n'), Some(ApprovalOption::Deny));

        let dynamic = ApprovalDialog {
            id: "approval-2".to_string(),
            interaction: None,
            tool: "web_search".to_string(),
            target: Some("query".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: ApprovalDialog::options_for("web_search", Some("query")),
            diff: None,
        };
        assert_eq!(dynamic.option_for_key('2'), None);
        assert_eq!(
            dynamic.option_for_key('3'),
            Some(ApprovalOption::AlwaysTool)
        );
    }

    #[test]
    fn approval_dialog_has_four_options_with_target_and_three_without() {
        // Static-target tool (like read_file) shows AlwaysTarget option.
        let with_target = ApprovalDialog::options_for("read_file", Some("src/auth/token.rs"));
        assert_eq!(
            with_target,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
        // No target — AlwaysTarget is hidden.
        let without = ApprovalDialog::options_for("read_file", None);
        assert_eq!(
            without,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
        // Dynamic-target tool (web_search) — AlwaysTarget is hidden even with a target.
        let dynamic = ApprovalDialog::options_for("web_search", Some("some query"));
        assert_eq!(
            dynamic,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
    }

    #[test]
    fn approval_allowlist_grants_matching_tool_and_target() {
        let mut tool_scope = state();

        // Initially nothing is allow-listed.
        assert!(!tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));

        // "Always allow tool" grants every target for that tool.
        tool_scope
            .approval_allowlist
            .insert(AppState::approval_key_tool("edit"));
        assert!(tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));
        assert!(tool_scope.approval_is_allowlisted("edit", Some("src/b.rs")));
        assert!(!tool_scope.approval_is_allowlisted("bash", Some("ls")));

        // "Always allow tool + target" is scoped to that one target.
        let mut scoped = state();
        scoped
            .approval_allowlist
            .insert(AppState::approval_key_target("bash", "cargo test"));
        assert!(scoped.approval_is_allowlisted("bash", Some("cargo test")));
        assert!(!scoped.approval_is_allowlisted("bash", Some("rm -rf /")));
    }

    #[test]
    fn approval_needed_event_populates_dialog_options_and_diff() {
        let mut state = state();
        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
            tool: "edit".to_string(),
            target: Some("src/auth/token.rs".to_string()),
            preview: Some("@@ token.rs @@\n- a\n+ b".to_string()),
        });
        let dialog = state.approval_dialog.expect("dialog present");
        assert_eq!(dialog.id, "approval-1");
        assert_eq!(dialog.options.len(), 4);
        assert!(dialog.diff.is_some());
        assert_eq!(dialog.current(), ApprovalOption::Once);
    }

    #[test]
    fn user_input_requested_event_tracks_pending_runtime_interaction_id() {
        let mut state = state();
        state.update(TuiEvent::UserInputRequested {
            key: interaction_key(TuiInteractionKind::UserInput, "ask-1"),
            question: "Continue?".to_string(),
            choices: vec!["yes".to_string(), "no".to_string()],
        });

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert!(matches!(
            state.pending_input.as_ref(),
            Some(PendingTuiInput::UserInput(key)) if key.request_id == "ask-1"
        ));
    }

    #[test]
    fn mcp_elicitation_requested_event_tracks_pending_runtime_interaction_id() {
        let mut state = state();
        state.update(TuiEvent::McpElicitationRequested {
            key: interaction_key(
                TuiInteractionKind::McpElicitation,
                "mcp_elicitation:github:42",
            ),
            server_name: "github".to_string(),
            mode: RuntimeMcpElicitationMode::Url,
            message: "Authorize GitHub".to_string(),
            url: Some("https://github.com/login/device".to_string()),
            requested_schema_json: None,
        });

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert!(matches!(
            state.pending_input.as_ref(),
            Some(PendingTuiInput::McpElicitation(key))
                if key.request_id == "mcp_elicitation:github:42"
        ));
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message.contains("MCP github requests input: Authorize GitHub")
                    && message.contains("Mode: url")
                    && message.contains("URL: https://github.com/login/device")
        ));
    }

    #[test]
    fn session_completion_clears_pending_interaction_projection() {
        let mut input_state = state();
        input_state.update(TuiEvent::UserInputRequested {
            key: interaction_key(TuiInteractionKind::UserInput, "ask-1"),
            question: "Continue?".to_string(),
            choices: Vec::new(),
        });

        input_state.update(TuiEvent::SessionCompleted {
            status: "interrupted".to_string(),
        });

        assert_eq!(input_state.status, AppStatus::Idle);
        assert!(input_state.pending_input.is_none());

        let mut approval_state = state();
        approval_state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });

        approval_state.update(TuiEvent::SessionCompleted {
            status: "interrupted".to_string(),
        });

        assert_eq!(approval_state.status, AppStatus::Idle);
        assert!(approval_state.approval_dialog.is_none());
    }

    #[test]
    fn subagent_events_update_existing_message() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        state.update(TuiEvent::SubagentCompleted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
            status: "completed".to_string(),
            output: Some("done".to_string()),
            error: None,
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                description,
                status,
                output,
                error,
                ..
            } => {
                assert_eq!(id, "agent-1");
                assert_eq!(description, "inspect repo");
                assert_eq!(status, "completed");
                assert_eq!(output.as_deref(), Some("done"));
                assert!(error.is_none());
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn subagent_progress_updates_existing_message_without_adding_rows() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        state.update(TuiEvent::SubagentProgress {
            id: "agent-1".to_string(),
            activity: "bash: echo child".to_string(),
            turn: Some(1),
            usage: None,
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                status,
                activity,
                activity_tail,
                turn,
                ..
            } => {
                assert_eq!(id, "agent-1");
                assert_eq!(status, "running");
                assert_eq!(activity.as_deref(), Some("bash: echo child"));
                assert_eq!(activity_tail, &vec!["bash: echo child".to_string()]);
                assert_eq!(*turn, Some(1));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn subagent_progress_retains_recent_activity_tail() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        for index in 1..=8 {
            state.update(TuiEvent::SubagentProgress {
                id: "agent-1".to_string(),
                activity: format!("activity {index}"),
                turn: Some(index),
                usage: None,
            });
        }

        match &state.messages[0] {
            ChatMessage::Subagent {
                activity_tail,
                turn,
                ..
            } => {
                assert_eq!(*turn, Some(8));
                assert_eq!(activity_tail.len(), 6);
                assert_eq!(
                    activity_tail.first().map(String::as_str),
                    Some("activity 3")
                );
                assert_eq!(activity_tail.last().map(String::as_str), Some("activity 8"));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn expand_toggle_flips_latest_live_subagent() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });

        assert!(state.toggle_latest_tool_output());
        match &state.messages[0] {
            ChatMessage::Subagent { expanded, .. } => assert!(*expanded),
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn completed_subagent_without_start_adds_message() {
        let mut state = state();

        state.update(TuiEvent::SubagentCompleted {
            id: "agent-2".to_string(),
            description: "review code".to_string(),
            status: "failed".to_string(),
            output: None,
            error: Some("boom".to_string()),
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                description,
                status,
                output,
                error,
                ..
            } => {
                assert_eq!(id, "agent-2");
                assert_eq!(description, "review code");
                assert_eq!(status, "failed");
                assert!(output.is_none());
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn generic_subagent_tool_events_do_not_create_tool_rows() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-subagent".to_string(),
            name: "subagent".to_string(),
            target: Some("inspect repo".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-subagent".to_string(),
            name: "subagent".to_string(),
            status: "completed".to_string(),
            output: "Subagent status: success".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });

        assert!(state.messages.is_empty());
    }

    #[test]
    fn plan_lives_in_panel_during_turn_and_archives_inline_on_completion() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            target: Some("2 items".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            status: "completed".to_string(),
            output: "Plan updated".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        state.update(TuiEvent::PlanUpdated {
            explanation: Some("starting".to_string()),
            plan: vec![
                PlanItem {
                    step: "Inspect".to_string(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    step: "Patch".to_string(),
                    status: PlanStatus::InProgress,
                },
            ],
        });

        // During the turn the plan only lives in the bottom panel, not the scrollback.
        assert!(state.messages.is_empty());
        assert!(state.current_plan().is_some());

        // When the turn completes the panel clears and the plan is archived inline.
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(state.current_plan().is_none());
        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::PlanUpdate { explanation, plan } => {
                assert_eq!(explanation.as_deref(), Some("starting"));
                assert_eq!(plan.len(), 2);
                assert_eq!(plan[1].step, "Patch");
            }
            other => panic!("expected plan update message, got {other:?}"),
        }
    }

    #[test]
    fn proposed_plan_tags_stream_as_dedicated_tui_message() {
        let mut state = state();

        state.update(TuiEvent::MessageDelta("Intro\n<proposed".to_string()));
        state.update(TuiEvent::MessageDelta(
            "_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro".to_string(),
        ));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert_eq!(state.messages.len(), 3);
        match &state.messages[0] {
            ChatMessage::Assistant(text) => assert_eq!(text, "Intro\n"),
            other => panic!("expected assistant preface, got {other:?}"),
        }
        match &state.messages[1] {
            ChatMessage::ProposedPlan(text) => assert_eq!(text, "# Plan\n- inspect\n"),
            other => panic!("expected proposed plan, got {other:?}"),
        }
        match &state.messages[2] {
            ChatMessage::Assistant(text) => assert_eq!(text, "\nOutro"),
            other => panic!("expected assistant postscript, got {other:?}"),
        }
    }

    #[test]
    fn failed_plan_update_marks_panel_stale_until_next_success() {
        let mut state = state();

        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::InProgress,
            }],
        });
        assert!(!state.plan_update_failed());

        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan-2".to_string(),
            name: "update_plan".to_string(),
            status: "failed".to_string(),
            output: "tool arguments failed schema validation".to_string(),
            diff: None,
            kind: Some("error".to_string()),
        });
        assert!(
            state.plan_update_failed(),
            "failed update must mark the panel stale"
        );
        assert!(
            state.current_plan().is_some(),
            "the stale plan stays visible"
        );

        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::Completed,
            }],
        });
        assert!(
            !state.plan_update_failed(),
            "a successful update clears the stale marker"
        );
    }

    #[test]
    fn turn_completion_clears_plan_stale_marker() {
        let mut state = state();
        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::Pending,
            }],
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            status: "failed".to_string(),
            output: "schema validation".to_string(),
            diff: None,
            kind: Some("error".to_string()),
        });
        assert!(state.plan_update_failed());

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(!state.plan_update_failed());
    }

    #[test]
    fn session_completion_finalizes_the_turn_and_freezes_it() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::MessageDelta("answer".to_string()));

        // Mid-turn nothing is finalized: the whole transcript is still live.
        assert_eq!(state.finalized_count, 0);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        // After completion every message is frozen.
        assert_eq!(state.finalized_count, state.messages.len());
        assert!(state.finalized_count > 0);
        assert!(
            state.plan_approval_dialog.is_none(),
            "update_plan is a task checklist, not a proposed plan"
        );
    }

    #[test]
    fn successful_plan_turn_opens_approval_only_for_current_proposed_plan() {
        let mut state = state();
        state.approval_mode = ApprovalMode::Plan;
        state.pre_plan_approval_mode = Some(ApprovalMode::AutoEdit);
        state.enter_running();
        state.update(TuiEvent::MessageDelta(
            "<proposed_plan>\n# Plan\n1. Inspect\n2. Implement\n</proposed_plan>".to_string(),
        ));

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let dialog = state
            .plan_approval_dialog
            .as_ref()
            .expect("completed proposed plan should request approval");
        assert!(dialog.plan.contains("# Plan"));
        assert_eq!(dialog.selected, 0);
        assert_eq!(state.status, AppStatus::Idle);
        assert!(!state.queued_autosend_enabled());

        state.plan_approval_dialog = None;
        state.resume_queued_follow_up_autosend();
        state.enter_running();
        state.update(TuiEvent::MessageDelta(
            "A clarification is still needed.".to_string(),
        ));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(
            state.plan_approval_dialog.is_none(),
            "a historical plan must not reopen approval"
        );
    }

    #[test]
    fn proposed_plan_outside_plan_mode_or_failed_turn_does_not_open_approval() {
        for (mode, status) in [
            (ApprovalMode::AutoEdit, "success"),
            (ApprovalMode::Plan, "failed"),
        ] {
            let mut state = state();
            state.approval_mode = mode;
            state.enter_running();
            state.update(TuiEvent::MessageDelta(
                "<proposed_plan>\n- inspect\n</proposed_plan>".to_string(),
            ));
            state.update(TuiEvent::SessionCompleted {
                status: status.to_string(),
            });
            assert!(state.plan_approval_dialog.is_none(), "{mode:?} {status}");
        }
    }

    #[test]
    fn settings_transition_remembers_and_restores_pre_plan_mode() {
        let mut state = state();
        state.approval_mode = ApprovalMode::FullAuto;

        state.update(TuiEvent::SettingsUpdated {
            model: "model".to_string(),
            reasoning_effort: orca_core::config::ReasoningEffort::High,
            approval_mode: ApprovalMode::Plan,
        });
        assert_eq!(state.pre_plan_approval_mode, Some(ApprovalMode::FullAuto));

        state.update(TuiEvent::SettingsUpdated {
            model: "model".to_string(),
            reasoning_effort: orca_core::config::ReasoningEffort::High,
            approval_mode: ApprovalMode::FullAuto,
        });
        assert_eq!(state.pre_plan_approval_mode, None);
    }

    #[test]
    fn complete_lines_mutate_only_the_active_assistant_tail_revision() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("first line\n".to_string()));
        let first_revision = state.message_revisions[0];
        state.update(TuiEvent::MessageDelta("second line\n".to_string()));
        assert_eq!(state.messages.len(), 1);
        assert_ne!(state.message_revisions[0], first_revision);

        let revisions = state.message_revisions.clone();
        state.update(TuiEvent::MessageDelta("hidden half".to_string()));
        assert_eq!(state.message_revisions, revisions);
    }

    #[test]
    fn blank_boundary_freezes_tail_revision_and_new_block_uses_new_tail() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("first\n\n".to_string()));
        assert!(matches!(
            &state.messages[..],
            [ChatMessage::AssistantChunk {
                text,
                trailing_blank: true,
            }] if text == "first\n\n"
        ));
        let frozen_revision = state.message_revisions[0];

        state.update(TuiEvent::MessageDelta("second\n".to_string()));
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Assistant(text)) if text == "second\n"
        ));
        assert_eq!(state.message_revisions[0], frozen_revision);
    }

    #[test]
    fn reconcile_assistant_response_replaces_frozen_chunks_and_open_tail() {
        let mut state = state();
        state.push_message(ChatMessage::User("prompt".to_string()));
        state.update(TuiEvent::MessageDelta("first paragraph\n\n".to_string()));
        state.update(TuiEvent::MessageDelta("second paragraph".to_string()));
        state.update(TuiEvent::ReasoningDelta("streamed thinking".to_string()));
        assert!(matches!(
            state.messages.as_slice(),
            [
                ChatMessage::User(_),
                ChatMessage::AssistantChunk { .. },
                ChatMessage::Assistant(_),
                ChatMessage::Reasoning(_),
            ]
        ));

        state.update(TuiEvent::AssistantResponseCompleted(
            Some("full answer\n\n".to_string()),
            Some("full reasoning".to_string()),
        ));

        // Frozen chunks and the open tail are both replaced by the completed
        // response instead of being left to duplicate it.
        assert!(matches!(
            state.messages.as_slice(),
            [
                ChatMessage::User(_),
                ChatMessage::Reasoning(reasoning),
                ChatMessage::AssistantChunk {
                    text,
                    trailing_blank: true,
                },
            ] if reasoning == "full reasoning" && text == "full answer\n\n"
        ));
    }

    #[test]
    fn reconcile_assistant_response_drops_pending_partial_line() {
        let mut state = state();
        state.push_message(ChatMessage::User("prompt".to_string()));
        // The stream ends mid-line: the assembler holds "stale tail" without any
        // newline, so it has not been rendered as a message yet.
        state.update(TuiEvent::MessageDelta("first paragraph\n\n".to_string()));
        state.update(TuiEvent::MessageDelta("stale tail".to_string()));
        assert!(matches!(
            state.messages.as_slice(),
            [ChatMessage::User(_), ChatMessage::AssistantChunk { .. }]
        ));

        state.update(TuiEvent::AssistantResponseCompleted(
            Some("full answer\n\n".to_string()),
            Some("full reasoning".to_string()),
        ));

        // The held partial line must not bleed into the completed response.
        assert!(matches!(
            state.messages.as_slice(),
            [
                ChatMessage::User(_),
                ChatMessage::Reasoning(reasoning),
                ChatMessage::AssistantChunk { text, .. },
            ] if reasoning == "full reasoning" && text == "full answer\n\n"
        ));
    }

    fn assistant_projection_text(messages: &[ChatMessage]) -> String {
        messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Assistant(text) | ChatMessage::AssistantChunk { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn assistant_stream_completion_flushes_partial_unicode_once() {
        let mut state = state();
        for delta in ["中", "文👍🏽e\u{301}", "\n尾", "行"] {
            state.update(TuiEvent::MessageDelta(delta.to_string()));
        }
        assert_eq!(
            assistant_projection_text(&state.messages),
            "中文👍🏽e\u{301}\n"
        );

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert_eq!(
            assistant_projection_text(&state.messages),
            "中文👍🏽e\u{301}\n尾行"
        );
        let revisions = state.message_revisions.clone();

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert_eq!(
            assistant_projection_text(&state.messages),
            "中文👍🏽e\u{301}\n尾行"
        );
        assert_eq!(state.message_revisions, revisions);
    }

    #[test]
    fn proposed_plan_boundaries_preserve_agent_source_order() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("Intro\n<proposed".to_string()));
        state.update(TuiEvent::MessageDelta(
            "_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro".to_string(),
        ));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let plan_index = state
            .messages
            .iter()
            .position(|message| matches!(message, ChatMessage::ProposedPlan(_)))
            .expect("proposed plan message");
        assert_eq!(
            assistant_projection_text(&state.messages[..plan_index]),
            "Intro\n"
        );
        assert_eq!(
            assistant_projection_text(&state.messages[plan_index + 1..]),
            "\nOutro"
        );
        assert!(matches!(
            &state.messages[plan_index],
            ChatMessage::ProposedPlan(text) if text == "# Plan\n- inspect\n"
        ));
    }

    #[test]
    fn tool_boundary_finishes_hidden_assistant_text_before_tool_row() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("hidden tail".to_string()));
        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "grep".to_string(),
            target: None,
        });

        assert!(matches!(
            &state.messages[..],
            [
                ChatMessage::Assistant(text),
                ChatMessage::ToolCall { id, .. }
            ] if text == "hidden tail" && id == "tool-1"
        ));
    }

    #[test]
    fn transcript_reset_discards_hidden_assistant_text() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("discard me".to_string()));
        assert!(state.messages.is_empty());

        state.clear_messages();
        state.update(TuiEvent::MessageDelta("visible\n".to_string()));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert_eq!(assistant_projection_text(&state.messages), "visible\n");
    }

    #[test]
    fn retaining_messages_reindexes_the_active_assistant_tail() {
        let mut state = state();
        state.push_message(ChatMessage::System("remove".to_string()));
        state.update(TuiEvent::MessageDelta("first\n".to_string()));
        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
        );

        state.update(TuiEvent::MessageDelta("second\n".to_string()));

        assert_eq!(state.messages.len(), 1);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Assistant(text)) if text == "first\nsecond\n"
        ));
    }

    #[test]
    fn system_notice_finishes_hidden_assistant_text_before_notice() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("hidden tail".to_string()));
        state.update(TuiEvent::Notice("notice".to_string()));

        assert!(matches!(
            &state.messages[..],
            [ChatMessage::Assistant(text), ChatMessage::System(notice)]
                if text == "hidden tail" && notice == "notice"
        ));
    }

    #[test]
    fn session_completion_without_receiving_tools_preserves_populated_render_cache() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("stable markdown".to_string()));
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false),
            |_, message, _, _, _, _| match message {
                ChatMessage::Assistant(text) => vec![ratatui::text::Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        assert_eq!(state.transcript_render_cache.populated_len(), 1);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert_eq!(state.transcript_render_cache.populated_len(), 1);
    }

    #[test]
    fn expand_toggle_only_affects_live_tools_not_flushed_ones() {
        let mut state = state();

        // Turn 1: a tool call that gets completed.
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: Some("a".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "hit".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        // Simulate the render loop flushing the settled prefix into scrollback: once
        // `flushed_count` covers the tool it is committed to the immutable scrollback.
        state.flushed_count = state.messages.len();

        // The flushed tool is frozen: `e` finds nothing in the (empty) live pane.
        assert!(!state.toggle_latest_tool_output());
        let ChatMessage::ToolCall { expanded, .. } = &state.messages[0] else {
            panic!("expected flushed tool call");
        };
        assert!(!expanded, "flushed tool must stay collapsed");

        // Turn 2: a new live tool call (beyond `flushed_count`) can be expanded.
        state.update(TuiEvent::ToolRequested {
            id: "t2".to_string(),
            name: "grep".to_string(),
            target: Some("b".to_string()),
        });
        assert!(state.toggle_latest_tool_output());
        let ChatMessage::ToolCall { expanded, .. } = state.messages.last().unwrap() else {
            panic!("expected live tool call");
        };
        assert!(expanded, "live tool should toggle expanded");
    }

    #[test]
    fn clearing_messages_resets_the_finalized_watermark() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(state.finalized_count > 0);

        state.messages.clear();
        state.finalized_count = 0;

        // Watermark must never dangle past the (now empty) message list.
        assert_eq!(state.finalized_count, 0);
        assert!(state.messages.is_empty());
    }

    #[test]
    fn backtrack_clamps_watermark_into_remaining_messages() {
        let mut state = state();
        state.messages.push(ChatMessage::User("first".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply".to_string()));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        let finalized_before = state.finalized_count;
        assert_eq!(finalized_before, 2);

        // A second user prompt starts a new live turn, then we backtrack it away.
        state.messages.push(ChatMessage::User("second".to_string()));
        state.remove_after_last_user();

        // Everything from the last user prompt onward is gone, and the watermark is
        // clamped so it can never exceed the remaining message count.
        assert!(state.finalized_count <= state.messages.len());
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn submission_rejection_removes_optimistic_user_and_returns_idle() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("before".to_string()));
        state.push_message(ChatMessage::User("review @gone.txt".to_string()));
        state.enter_running();
        state.update(TuiEvent::ToolCallProgress {
            id: "receiving".to_string(),
            name: Some("read_file".to_string()),
            arguments_bytes: 128,
        });

        state.update(TuiEvent::SubmissionRejected {
            queued_id: None,
            prompt: "review @gone.txt".to_string(),
            message: "bound file is no longer available".to_string(),
        });

        assert_eq!(state.status, AppStatus::Idle);
        assert!(matches!(
            state.messages.as_slice(),
            [ChatMessage::Assistant(before), ChatMessage::Error(error)]
                if before == "before" && error == "bound file is no longer available"
        ));
        assert!(state.mention_bindings.is_empty());
        assert_eq!(state.running_started_at, None);
        assert!(state.messages.iter().all(|message| {
            !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
        }));
    }

    #[test]
    fn generic_error_does_not_end_a_running_turn() {
        let mut state = state();
        state.enter_running();

        state.update(TuiEvent::Error("recoverable runtime error".to_string()));

        assert_eq!(state.status, AppStatus::Running);
        assert!(state.running_started_at.is_some());
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message)) if message == "recoverable runtime error"
        ));
    }

    #[test]
    fn operation_rejection_reports_error_and_returns_idle() {
        let mut state = state();
        state.enter_running();

        state.update(TuiEvent::OperationRejected(
            "operation could not start".to_string(),
        ));

        assert_eq!(state.status, AppStatus::Idle);
        assert_eq!(state.running_started_at, None);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Error(message)) if message == "operation could not start"
        ));
    }

    #[test]
    fn recovery_projection_is_not_overwritten_by_lifecycle_events() {
        let mut state = state();
        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 3,
        ])
        .unwrap();

        let projection = |next_seq: u64, recoverable_operation_id: Option<SurfaceOperationId>| {
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(next_seq),
                session_id: Some("recovery-session".to_string()),
                title: "Recovery session".to_string(),
                usage_revision: next_seq,
                usage: UsageTotals::default(),
                context_revision: next_seq,
                context_used_tokens: 0,
                context_limit_tokens: 128_000,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: recoverable_operation_id.clone(),
                recoverable_operation_id,
                goal_presentation: None,
                session_presentation: None,
            }
        };

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            1,
            Some(operation_id.clone()),
        ))));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_id));
        assert!(state.recovery_prompt_visible);

        state.update(TuiEvent::TurnStarted {
            turn: 2,
            task: None,
        });
        assert_eq!(state.recoverable_operation_id(), Some(&operation_id));

        state.update(TuiEvent::HistoryLoaded {
            messages: Vec::new(),
            plan: None,
            label: "Resumed saved conversation.".to_string(),
        });
        state.update(TuiEvent::SessionCompleted {
            status: "cancelled".to_string(),
        });
        assert_eq!(state.recoverable_operation_id(), Some(&operation_id));
        assert!(state.recovery_prompt_visible);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            2, None,
        ))));
        assert!(state.recoverable_operation_id().is_none());
        assert!(!state.recovery_prompt_visible);
    }

    #[test]
    fn surface_operation_projection_fences_conflicts_and_resets() {
        let mut state = state();
        let operation_a = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 4,
        ])
        .unwrap();
        let operation_b = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 5,
        ])
        .unwrap();
        let projection =
            |next_seq: u64, operation: Option<SurfaceOperationId>| SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(next_seq),
                session_id: Some("operation-session".to_string()),
                title: "Operation session".to_string(),
                usage_revision: next_seq,
                usage: UsageTotals::default(),
                context_revision: next_seq,
                context_used_tokens: 0,
                context_limit_tokens: 128_000,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: operation.clone(),
                recoverable_operation_id: operation,
                goal_presentation: None,
                session_presentation: None,
            };
        let recovery_notice_count = |state: &AppState| {
            state
                .messages
                .iter()
                .filter(|message| {
                    matches!(message, ChatMessage::System(text) if text.starts_with("A recoverable operation is suspended."))
                })
                .count()
        };

        let accepted = projection(1, Some(operation_a.clone()));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            accepted.clone(),
        )));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(accepted)));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
        assert_eq!(recovery_notice_count(&state), 1);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            1,
            Some(operation_b.clone()),
        ))));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
        assert_eq!(recovery_notice_count(&state), 1);

        let mut mismatched = projection(2, Some(operation_b.clone()));
        mismatched.foreground_operation_id = Some(operation_a.clone());
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(mismatched)));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
        assert_eq!(recovery_notice_count(&state), 1);

        let mut mismatched_reset = projection(2, Some(operation_b.clone()));
        mismatched_reset.foreground_operation_id = Some(operation_a.clone());
        mismatched_reset.session_id = Some("invalid-reset-session".to_string());
        state.update(TuiEvent::SessionProjectionReset(Box::new(mismatched_reset)));
        assert_eq!(state.current_session_id(), Some("operation-session"));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_a));
        assert_eq!(recovery_notice_count(&state), 1);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            2,
            Some(operation_b.clone()),
        ))));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_b));
        assert_eq!(recovery_notice_count(&state), 2);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            3, None,
        ))));
        assert!(state.recoverable_operation_id().is_none());
        assert!(!state.recovery_prompt_visible);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            2,
            Some(operation_b.clone()),
        ))));
        assert!(state.recoverable_operation_id().is_none());

        let mut other_session = projection(4, Some(operation_b.clone()));
        other_session.cursor.incarnation =
            orca_runtime::surface::SurfaceIncarnation::try_from_bytes([
                0x02, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 6,
            ])
            .unwrap();
        other_session.session_id = Some("other-operation-session".to_string());
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            other_session.clone(),
        )));
        assert!(state.recoverable_operation_id().is_none());

        state.update(TuiEvent::SessionProjectionReset(Box::new(other_session)));
        assert_eq!(state.current_session_id(), Some("other-operation-session"));
        assert_eq!(state.recoverable_operation_id(), Some(&operation_b));
        assert!(state.recovery_prompt_visible);
    }

    #[test]
    fn flushable_prefix_stops_at_a_running_tool_call() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: Some("a".to_string()),
        });
        // User is settled, the running tool blocks everything after it.
        assert_eq!(state.flushable_prefix_end(false), 1);

        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "hit".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        // Now the completed tool can flush too.
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn flushable_prefix_stops_at_a_receiving_tool_call() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::ToolCallProgress {
            id: "t1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 1024,
        });

        assert_eq!(state.flushable_prefix_end(false), 1);
    }

    #[test]
    fn flushable_prefix_excludes_hidden_partial_until_completion_flushes_it() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::MessageDelta("partial".to_string()));

        // The partial source line is still hidden, so only the user prompt exists.
        assert_eq!(state.flushable_prefix_end(false), 1);
        assert_eq!(state.flushable_prefix_end(true), 1);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        // Completion flushes the hidden line as a finalized assistant message.
        assert_eq!(state.flushable_prefix_end(true), 2);
    }

    #[test]
    fn flushable_prefix_releases_an_assistant_block_once_a_newer_message_follows() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("first answer".to_string()));
        // While it is the last message it is still mutable.
        assert_eq!(state.flushable_prefix_end(false), 0);

        // A following tool call means the assistant block will never grow again.
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: None,
        });
        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "out".to_string(),
            diff: None,
            kind: None,
        });
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn flushable_prefix_is_bounded_by_already_flushed_count() {
        let mut state = state();
        state.messages.push(ChatMessage::User("a".to_string()));
        state.messages.push(ChatMessage::System("b".to_string()));
        state.flushed_count = 1;
        // Counts the contiguous settled run starting from flushed_count, not from 0.
        assert_eq!(state.flushable_prefix_end(false), 2);

        state.flushed_count = 2;
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn session_completion_re_pins_to_bottom_after_incidental_scroll() {
        let mut state = state();
        state.enter_running();
        state.total_lines = 100;
        state.visible_height = 20;
        state.scroll_offset = 60;
        state.auto_scroll = false;
        state
            .messages
            .push(ChatMessage::Assistant("final answer".to_string()));

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert!(
            state.auto_scroll,
            "finished turns should leave the final answer pinned above the composer"
        );
        assert_eq!(state.scroll_offset, 80);
    }

    #[test]
    fn scroll_up_with_content_shorter_than_pane_keeps_auto_follow() {
        // First screen: everything fits, nothing to scroll. A stray wheel-up (trackpad
        // inertia, accidental touch) must not disarm auto-follow, or the transcript
        // stops tracking new streamed content once it grows past one screen and the
        // user is forced to scroll down by hand.
        let mut state = state();
        state.total_lines = 10;
        state.visible_height = 24;
        state.auto_scroll = true;

        state.scroll_up(3);

        assert!(
            state.auto_scroll,
            "wheel-up on a not-yet-overflowing transcript must keep auto-follow armed"
        );
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn scroll_up_with_overflow_disarms_auto_follow() {
        let mut state = state();
        state.total_lines = 100;
        state.visible_height = 24;
        state.scroll_offset = 76;
        state.auto_scroll = true;

        state.scroll_up(3);

        assert!(
            !state.auto_scroll,
            "wheel-up on an overflowing transcript should still let the user break away"
        );
        assert_eq!(state.scroll_offset, 73);
    }

    #[test]
    fn scroll_navigation_preserves_offsets_above_u16_max() {
        let mut state = state();
        state.total_lines = 100_000;
        state.visible_height = 20;
        state.scroll_offset = 70_000;
        state.auto_scroll = false;

        state.scroll_down(5_000usize);
        assert_eq!(state.scroll_offset, 75_000);
        state.scroll_up(10_000usize);
        assert_eq!(state.scroll_offset, 65_000);
    }

    #[test]
    fn scroll_down_saturates_when_total_height_reaches_usize_max() {
        let mut state = state();
        state.total_lines = usize::MAX;
        state.visible_height = 0;
        state.scroll_offset = usize::MAX - 1;
        state.auto_scroll = false;

        state.scroll_down(10usize);

        assert_eq!(state.scroll_offset, usize::MAX);
        assert!(state.auto_scroll);
    }

    #[test]
    fn session_completion_temporarily_ignores_inertial_mouse_scroll() {
        let mut state = state();
        state.enter_running();
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let completed_at = state
            .last_completed_at
            .expect("session completion should record completion time");

        assert!(
            !state.accepts_mouse_scroll_at(completed_at),
            "trackpad inertia immediately after completion must not undo bottom pinning"
        );
        assert!(
            state.accepts_mouse_scroll_at(completed_at + std::time::Duration::from_millis(900)),
            "manual mouse scrolling should work again after the completion grace period"
        );
    }

    #[test]
    fn usage_projection_allows_compaction_drop_and_rejects_stale_revision() {
        let mut state = state();
        let before_compaction = UsageTotals {
            input_tokens: 50_000,
            output_tokens: 800,
            cache_tokens: 400,
            estimated_cost_usd: 0.03,
        };
        let after_compaction = UsageTotals {
            input_tokens: 8_000,
            output_tokens: 900,
            cache_tokens: 450,
            estimated_cost_usd: 0.035,
        };
        let stale = UsageTotals {
            input_tokens: 60_000,
            output_tokens: 700,
            cache_tokens: 350,
            estimated_cost_usd: 0.025,
        };

        let projection = |usage_revision, usage| SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(usage_revision),
            session_id: Some("usage-session".to_string()),
            title: "Usage session".to_string(),
            usage_revision,
            usage,
            context_revision: 1,
            context_used_tokens: 8_000,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        };

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            10,
            before_compaction,
        ))));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            11,
            after_compaction.clone(),
        ))));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            9, stale,
        ))));

        assert_eq!(state.usage(), &after_compaction);
    }

    #[test]
    fn surface_session_projection_fences_stale_and_cross_thread_identity() {
        let mut state = state();
        let projection = |cursor, session_id: &str, title: &str| SurfaceProjectionState {
            cursor,
            session_id: Some(session_id.to_string()),
            title: title.to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        };
        let committed = projection(
            crate::surface_projection::test_surface_cursor(2),
            "session-1",
            "Committed title",
        );
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(committed)));

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(1),
            "session-1",
            "Stale title",
        ))));
        assert_eq!(
            state.current_session_title(),
            Some("Committed title"),
            "an older cursor must not overwrite the accepted title"
        );

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(2),
            "session-1",
            "Contradictory title",
        ))));
        assert_eq!(
            state.current_session_title(),
            Some("Committed title"),
            "an equal cursor with contradictory identity must be rejected"
        );

        let mut different_cursor = crate::surface_projection::test_surface_cursor(1);
        let mut thread_bytes = [9; 16];
        thread_bytes[6] = 0x79;
        thread_bytes[8] = 0x89;
        different_cursor.thread_id =
            orca_runtime::surface::SurfaceThreadId::try_from_bytes(thread_bytes)
                .expect("different test surface thread id");
        let mut different_projection =
            projection(different_cursor, "session-2", "Different thread");
        different_projection.usage.input_tokens = 99;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            different_projection.clone(),
        )));
        assert_eq!(
            state.current_session_id(),
            Some("session-1"),
            "ordinary projection cannot switch threads"
        );
        assert_eq!(
            state.usage().input_tokens,
            0,
            "rejected identity must reject the whole projection envelope"
        );

        state.update(TuiEvent::SessionProjectionReset(Box::new(
            different_projection,
        )));
        assert_eq!(state.current_session_id(), Some("session-2"));
        assert_eq!(state.current_session_title(), Some("Different thread"));
        assert_eq!(state.usage().input_tokens, 99);
    }

    #[test]
    fn surface_session_projection_presents_once_per_cursor() {
        let mut state = state();
        let projection = |next_seq, title: &str, session_presentation| SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(next_seq),
            session_id: Some("session-1".to_string()),
            title: title.to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation,
        };
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            1,
            "Initial title",
            None,
        ))));
        let renamed = projection(
            2,
            "Committed title",
            Some(crate::surface_projection::SessionProjectionPresentation::Renamed),
        );
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(renamed.clone())));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(renamed.clone())));
        let duplicate_directive = SurfaceProjectionState {
            session_presentation: Some(
                crate::surface_projection::SessionProjectionPresentation::Forked,
            ),
            ..renamed
        };
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            duplicate_directive,
        )));

        assert_eq!(state.current_session_title(), Some("Committed title"));
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        ChatMessage::System(text)
                            if text == "Renamed conversation to Committed title."
                    )
                })
                .count(),
            1
        );
        assert!(!state.messages.iter().any(|message| {
            matches!(message, ChatMessage::System(text) if text.starts_with("Forked conversation"))
        }));
    }

    #[test]
    fn rejected_reset_preserves_existing_surface_state() {
        let mut state = state();
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("session-1".to_string()),
                title: "Existing title".to_string(),
                usage_revision: 1,
                usage: UsageTotals {
                    input_tokens: 7,
                    ..UsageTotals::default()
                },
                context_revision: 1,
                context_used_tokens: 3,
                context_limit_tokens: 100,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
            },
        )));
        state.push_message(ChatMessage::User("keep me".to_string()));

        state.update(TuiEvent::SessionProjectionReset(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(2),
                session_id: None,
                title: "Invalid ephemeral rename".to_string(),
                usage_revision: 2,
                usage: UsageTotals::default(),
                context_revision: 2,
                context_used_tokens: 0,
                context_limit_tokens: 0,
                workflow_tasks: Vec::new(),
                current_goal: None,
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: Some(
                    crate::surface_projection::SessionProjectionPresentation::Renamed,
                ),
            },
        )));

        assert_eq!(state.current_session_id(), Some("session-1"));
        assert_eq!(state.current_session_title(), Some("Existing title"));
        assert_eq!(state.usage().input_tokens, 7);
        assert!(matches!(
            state.messages.as_slice(),
            [ChatMessage::User(prompt)] if prompt == "keep me"
        ));
    }

    #[test]
    fn surface_goal_projection_rejects_equal_usage_stale_snapshot() {
        let mut state = state();
        let committed = ThreadGoal {
            session_id: "goal-session".to_string(),
            objective: "new objective".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Paused,
            token_budget: Some(10_000),
            tokens_used: 100,
            time_used_seconds: 10,
            created_at: 1,
            updated_at: 3,
        };
        let stale = ThreadGoal {
            objective: "old objective".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            tokens_used: 10,
            time_used_seconds: 1,
            updated_at: 2,
            ..committed.clone()
        };
        let projection = |cursor, goal, goal_presentation| SurfaceProjectionState {
            cursor,
            session_id: Some("goal-session".to_string()),
            title: "Goal session".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: goal,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation,
            session_presentation: None,
        };

        let committed_projection = projection(
            crate::surface_projection::test_surface_cursor(2),
            Some(committed.clone()),
            Some(crate::surface_projection::GoalProjectionPresentation::Updated),
        );
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            committed_projection.clone(),
        )));
        let goal_notice_count = |state: &AppState| {
            state
                .messages
                .iter()
                .filter(|message| {
                    matches!(message, ChatMessage::System(text) if text.contains("new objective"))
                })
                .count()
        };
        assert_eq!(goal_notice_count(&state), 1);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            committed_projection.clone(),
        )));
        assert_eq!(goal_notice_count(&state), 1, "equal replay is silent");

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(2),
            Some(stale.clone()),
            Some(crate::surface_projection::GoalProjectionPresentation::Updated),
        ))));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(1),
            Some(stale.clone()),
            None,
        ))));

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(3),
            Some(committed.clone()),
            None,
        ))));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            crate::surface_projection::test_surface_cursor(2),
            Some(stale.clone()),
            Some(crate::surface_projection::GoalProjectionPresentation::Updated),
        ))));

        let mut different_incarnation = crate::surface_projection::test_surface_cursor(4);
        let mut incarnation_bytes = [5; 16];
        incarnation_bytes[6] = 0x75;
        incarnation_bytes[8] = 0x85;
        different_incarnation.incarnation =
            orca_runtime::surface::SurfaceIncarnation::try_from_bytes(incarnation_bytes)
                .expect("different test surface incarnation");
        let after_reset = ThreadGoal {
            objective: "accepted after reset".to_string(),
            updated_at: 4,
            ..stale
        };
        let different_incarnation_projection = projection(
            different_incarnation,
            Some(after_reset.clone()),
            Some(crate::surface_projection::GoalProjectionPresentation::Updated),
        );
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            different_incarnation_projection.clone(),
        )));

        assert_eq!(state.current_goal(), Some(&committed));
        assert_eq!(goal_notice_count(&state), 1);

        state.update(TuiEvent::SessionProjectionReset(Box::new(
            different_incarnation_projection,
        )));
        assert_eq!(state.current_goal(), Some(&after_reset));
    }

    #[test]
    fn surface_goal_projection_hydration_is_silent() {
        let mut state = state();
        let goal = ThreadGoal {
            session_id: "goal-session".to_string(),
            objective: "hydrate without a notice".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Paused,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 2,
        };
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("goal-session".to_string()),
                title: "Goal session".to_string(),
                usage_revision: 1,
                usage: UsageTotals::default(),
                context_revision: 1,
                context_used_tokens: 0,
                context_limit_tokens: 128_000,
                workflow_tasks: Vec::new(),
                current_goal: Some(goal.clone()),
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: None,
                session_presentation: None,
            },
        )));

        assert_eq!(state.current_goal(), Some(&goal));
        assert!(state.messages.is_empty());
    }

    #[test]
    fn surface_goal_projection_presents_clear_once_per_cursor() {
        let mut state = state();
        let goal = ThreadGoal {
            session_id: "goal-session".to_string(),
            objective: "clear me".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Paused,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 2,
        };
        let projection = |next_seq, goal, goal_presentation| SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(next_seq),
            session_id: Some("goal-session".to_string()),
            title: "Goal session".to_string(),
            usage_revision: 1,
            usage: UsageTotals::default(),
            context_revision: 1,
            context_used_tokens: 0,
            context_limit_tokens: 128_000,
            workflow_tasks: Vec::new(),
            current_goal: goal,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation,
            session_presentation: None,
        };
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection(
            1,
            Some(goal),
            None,
        ))));
        let cleared = projection(
            2,
            None,
            Some(crate::surface_projection::GoalProjectionPresentation::Cleared),
        );
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(cleared.clone())));
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(cleared)));

        assert!(state.current_goal().is_none());
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|message| {
                    matches!(message, ChatMessage::System(text) if text == "Goal cleared.")
                })
                .count(),
            1
        );
    }

    #[test]
    fn surface_projection_consistency_current_goal_reconciles_session_scoped_state() {
        let mut state = state();
        state.replace_session_identity_for_test(
            Some("stale-session".to_string()),
            Some("stale title".to_string()),
        );
        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            target: None,
        });

        let operation_id = SurfaceOperationId::try_from_bytes([
            0x01, 0x8f, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1,
        ])
        .expect("valid surface operation id");
        let goal = ThreadGoal {
            session_id: "canonical-session".to_string(),
            objective: "keep the projection canonical".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: Some(10_000),
            tokens_used: 42,
            time_used_seconds: 3,
            created_at: 1,
            updated_at: 2,
        };
        let expected = SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(7),
            session_id: Some("canonical-session".to_string()),
            title: "canonical title".to_string(),
            usage_revision: 7,
            usage: UsageTotals {
                input_tokens: 700,
                output_tokens: 70,
                cache_tokens: 7,
                estimated_cost_usd: 0.007,
            },
            context_revision: 1,
            context_used_tokens: 700,
            context_limit_tokens: 1_000,
            workflow_tasks: vec![workflow_task_summary("task-1", "Canonical task")],
            current_goal: Some(goal),
            foreground_operation_id: Some(operation_id),
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        };

        state.update(TuiEvent::SessionProjectionReset(Box::new(expected.clone())));

        assert_eq!(state.current_session_id(), Some("canonical-session"));
        assert_eq!(state.current_session_title(), Some("canonical title"));
        assert_eq!(state.usage(), &expected.usage);
        assert_eq!(state.context_used_tokens(), expected.context_used_tokens);
        assert_eq!(state.context_limit_tokens(), expected.context_limit_tokens);
        assert_eq!(state.workflow_panel.tasks, expected.workflow_tasks);
        assert_eq!(state.current_goal(), expected.current_goal.as_ref());
        assert_eq!(
            state.foreground_operation_id(),
            expected.foreground_operation_id.as_ref()
        );
        state.assert_surface_projection_consistent(&expected);

        let mut same_context_revision = expected.clone();
        same_context_revision.context_used_tokens = 25_000;
        same_context_revision.context_limit_tokens = 1_000_000;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            same_context_revision,
        )));
        assert_eq!(state.context_used_tokens(), expected.context_used_tokens);
        assert_eq!(state.context_limit_tokens(), expected.context_limit_tokens);

        let mut compacted = expected.clone();
        compacted.context_revision = 2;
        compacted.context_used_tokens = 10_000;
        compacted.context_limit_tokens = 1_000_000;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            compacted.clone(),
        )));
        assert_eq!(state.context_used_tokens(), 10_000);
        assert_eq!(state.context_limit_tokens(), 1_000_000);

        let mut newer_usage_with_stale_context = compacted;
        newer_usage_with_stale_context.usage_revision = 8;
        newer_usage_with_stale_context.usage.input_tokens = 800;
        newer_usage_with_stale_context.context_revision = 1;
        newer_usage_with_stale_context.context_used_tokens = 50_000;
        newer_usage_with_stale_context.context_limit_tokens = 128_000;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            newer_usage_with_stale_context,
        )));
        assert_eq!(state.usage().input_tokens, 800);
        assert_eq!(state.context_used_tokens(), 10_000);
        assert_eq!(state.context_limit_tokens(), 1_000_000);

        let mut stale = expected.clone();
        stale.usage_revision = 6;
        stale.title = "stale projection title".to_string();
        stale.usage.input_tokens = 1;
        stale.foreground_operation_id = None;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(stale)));
        assert_eq!(state.current_session_title(), Some("canonical title"));
        assert_eq!(state.usage().input_tokens, 800);
        assert_eq!(
            state.foreground_operation_id(),
            expected.foreground_operation_id.as_ref()
        );

        let mut next_session = expected.clone();
        next_session.session_id = Some("next-session".to_string());
        next_session.title = "next title".to_string();
        next_session.usage_revision = 1;
        next_session.usage.input_tokens = 12;
        next_session.context_revision = 1;
        next_session.context_used_tokens = 12_000;
        next_session.context_limit_tokens = 256_000;
        next_session.workflow_tasks.clear();
        next_session.current_goal = None;
        next_session.foreground_operation_id = None;
        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(next_session)));
        assert_eq!(state.usage().input_tokens, 800);

        let mut empty = expected;
        empty.cursor = crate::surface_projection::test_surface_cursor(9);
        empty.session_id = Some("empty-session".to_string());
        empty.title = "empty title".to_string();
        empty.usage_revision = 1;
        empty.usage = UsageTotals::default();
        empty.context_revision = 1;
        empty.context_used_tokens = 0;
        empty.context_limit_tokens = 0;
        empty.workflow_tasks.clear();
        empty.current_goal = None;
        empty.foreground_operation_id = None;
        state.update(TuiEvent::SessionProjectionReset(Box::new(empty)));
        assert_eq!(state.usage(), &UsageTotals::default());
        assert_eq!(state.context_used_tokens(), 0);
        assert_eq!(state.context_limit_tokens(), 0);
    }

    #[test]
    fn backtrack_clamps_flushed_watermark_too() {
        let mut state = state();
        state.messages.push(ChatMessage::User("first".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply".to_string()));
        state.flushed_count = 2;
        state.finalized_count = 2;

        state.messages.push(ChatMessage::User("second".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply2".to_string()));
        state.remove_after_last_user();

        assert!(state.flushed_count <= state.messages.len());
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn completed_tool_event_preserves_result_kind() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "grep-1".to_string(),
            name: "grep".to_string(),
            target: Some("needle".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "grep-1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "(no matches)".to_string(),
            diff: None,
            kind: Some("no_matches".to_string()),
        });

        match &state.messages[0] {
            ChatMessage::ToolCall { kind, .. } => {
                assert_eq!(kind.as_deref(), Some("no_matches"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_index_matches_canonical_scan_after_mutations() {
        fn tool_call(id: &str) -> ChatMessage {
            ChatMessage::ToolCall {
                id: id.to_string(),
                name: "bash".to_string(),
                target: None,
                status: "completed".to_string(),
                output: None,
                diff: None,
                kind: None,
                expanded: false,
            }
        }

        fn assert_matches_canonical_scan(state: &AppState, ids: &[&str]) {
            state.assert_tool_call_index_consistent();
            for id in ids {
                let canonical = state.messages.iter().position(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == id)
                });
                assert_eq!(state.tool_call_message_index(id), canonical, "id={id}");
            }
        }

        let mut state = state();
        state.push_message(tool_call("first"));
        state.push_message(ChatMessage::System("between".to_string()));
        state.push_message(tool_call("duplicate"));
        state.push_message(tool_call("duplicate"));
        assert_matches_canonical_scan(&state, &["first", "duplicate", "missing"]);

        assert!(state.replace_message(0, tool_call("replacement")));
        assert_matches_canonical_scan(&state, &["first", "replacement", "duplicate", "missing"]);

        state.truncate_messages(3);
        assert_matches_canonical_scan(&state, &["replacement", "duplicate"]);

        state.retain_messages(|message| !matches!(message, ChatMessage::System(_)));
        assert_matches_canonical_scan(&state, &["replacement", "duplicate"]);

        state.replace_messages([tool_call("history"), tool_call("history")]);
        assert_matches_canonical_scan(&state, &["replacement", "history"]);

        state.clear_messages();
        assert_matches_canonical_scan(&state, &["history"]);
    }

    #[test]
    fn tool_output_delta_updates_matching_tool_id() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "a".to_string(),
            name: "bash".to_string(),
            target: Some("first".to_string()),
        });
        state.update(TuiEvent::ToolRequested {
            id: "b".to_string(),
            name: "bash".to_string(),
            target: Some("second".to_string()),
        });
        state.update(TuiEvent::ToolOutputDelta {
            id: "a".to_string(),
            chunk: "one\n".to_string(),
        });

        match &state.messages[0] {
            ChatMessage::ToolCall { output, .. } => {
                assert_eq!(output.as_deref(), Some("one\n"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        match &state.messages[1] {
            ChatMessage::ToolCall { output, .. } => assert!(output.is_none()),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn completed_tool_event_replaces_live_preview_with_canonical_output() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "bash-preview".to_string(),
            name: "bash".to_string(),
            target: Some("printf output".to_string()),
        });
        state.update(TuiEvent::ToolOutputDelta {
            id: "bash-preview".to_string(),
            chunk: "live preview".to_string(),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "bash-preview".to_string(),
            name: "bash".to_string(),
            status: "completed".to_string(),
            output: "canonical bounded output".to_string(),
            diff: None,
            kind: None,
        });

        match &state.messages[0] {
            ChatMessage::ToolCall { output, .. } => {
                assert_eq!(output.as_deref(), Some("canonical bounded output"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_progress_creates_and_updates_running_row() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        });
        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::ToolRequested {
            id: "call_1".to_string(),
            name: "write_file".to_string(),
            target: Some("big.js".to_string()),
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::ToolCall {
                name,
                target,
                status,
                output,
                ..
            } => {
                assert_eq!(name, "write_file");
                assert_eq!(target.as_deref(), Some("big.js"));
                assert_eq!(status, "running");
                assert_eq!(output.as_deref(), Some("receiving arguments... 24.1 KB"));
            }
            other => panic!("expected tool progress row, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_progress_ignores_panel_owned_tools() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "plan-1".to_string(),
            name: Some("update_plan".to_string()),
            arguments_bytes: 1024,
        });
        state.update(TuiEvent::ToolCallProgress {
            id: "subagent-1".to_string(),
            name: Some("subagent".to_string()),
            arguments_bytes: 2048,
        });

        assert!(state.messages.is_empty());
    }

    #[test]
    fn terminal_events_remove_orphan_receiving_tool_progress() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        });
        state.update(TuiEvent::Error("failed to parse tool call".to_string()));

        assert!(
            state.messages.iter().all(|message| {
                !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            }),
            "error should clear orphan receiving rows: {:?}",
            state.messages
        );

        state.update(TuiEvent::ToolCallProgress {
            id: "call_2".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::SessionCompleted {
            status: "cancelled".to_string(),
        });

        assert!(
            state.messages.iter().all(|message| {
                !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            }),
            "completion should clear orphan receiving rows: {:?}",
            state.messages
        );
    }

    #[test]
    fn clearing_receiving_progress_preserves_finalized_prefix_boundaries() {
        let mut state = state();
        state.messages.push(ChatMessage::ToolCall {
            id: "frozen".to_string(),
            name: "write_file".to_string(),
            target: None,
            status: "receiving".to_string(),
            output: Some("receiving arguments... 1 KB".to_string()),
            diff: None,
            kind: None,
            expanded: false,
        });
        state.finalized_count = 1;
        state.flushed_count = 1;

        state.update(TuiEvent::ToolCallProgress {
            id: "live".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::Error("failed".to_string()));

        assert_eq!(state.finalized_count, 1);
        assert_eq!(state.flushed_count, 1);
        assert_eq!(state.messages.len(), 2);
        match &state.messages[0] {
            ChatMessage::ToolCall { id, status, .. } => {
                assert_eq!(id, "frozen");
                assert_eq!(status, "receiving");
            }
            other => panic!("finalized prefix should be preserved, got {other:?}"),
        }
        assert!(matches!(state.messages[1], ChatMessage::Error(_)));
    }

    #[test]
    fn toggle_latest_tool_output_flips_expanded_state() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "grep".to_string(),
            target: None,
        });

        assert!(state.toggle_latest_tool_output());
        match &state.messages[0] {
            ChatMessage::ToolCall { expanded, .. } => assert!(*expanded),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn workflow_panel_state_defaults_to_empty() {
        let state = state();

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert_eq!(state.workflow_panel.selected, 0);
        assert!(state.workflow_panel.tasks.is_empty());
    }

    #[test]
    fn show_workflows_preserves_available_selection() {
        let mut state = state();
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "demo".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        }];
        state.workflow_panel.selected = 9;

        state.show_workflows();

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflow_panel_selection_moves_within_available_tasks() {
        let mut state = state();
        state.workflow_panel.tasks = vec![
            workflow_task_summary("task-1", "audit"),
            workflow_task_summary("task-2", "repair"),
        ];

        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 1);

        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 1);

        state.select_previous_workflow_task();
        assert_eq!(state.workflow_panel.selected, 0);

        state.workflow_panel.tasks.clear();
        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn selected_background_approval_task_opens_approval_dialog() {
        let mut state = state();
        let mut task = workflow_task_summary("task-approval", "approval");
        task.task_type = TaskType::MainSession;
        task.status = TaskStatus::ApprovalRequired;
        task.is_backgrounded = true;
        task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "mock-tool-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: Some("background task".to_string()),
            arguments: "{\"limit\":1}".to_string(),
        });
        state.workflow_panel.tasks = vec![task];

        assert!(state.open_selected_background_approval_dialog());

        assert_eq!(state.status, AppStatus::WaitingApproval);
        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.tool, "task_list");
        assert_eq!(dialog.target.as_deref(), Some("background task"));
        assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
        assert_eq!(dialog.diff.as_deref(), Some("{\"limit\":1}"));
    }

    #[test]
    fn foreground_claimed_background_approval_can_reopen_dialog() {
        let mut state = state();
        let mut task = workflow_task_summary("task-approval", "approval");
        task.task_type = TaskType::MainSession;
        task.status = TaskStatus::ApprovalRequired;
        task.is_backgrounded = false;
        task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "mock-tool-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: Some("foreground claimed task".to_string()),
            arguments: "{}".to_string(),
        });
        state.workflow_panel.tasks = vec![task];

        assert!(state.open_selected_background_approval_dialog());
        assert_eq!(
            state
                .approval_dialog
                .as_ref()
                .and_then(|dialog| dialog.background_task_id.as_deref()),
            Some("task-approval")
        );
    }

    #[test]
    fn show_agents_uses_dedicated_panel_mode() {
        let mut state = state();
        state.workflow_panel.selected = 9;

        state.show_agents();

        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflow_events_update_panel_and_queue_model_notification() {
        let mut state = state();
        state.workflow_panel.selected = 9;

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-1".to_string(),
                task_type: TaskType::Workflow,
                status: TaskStatus::Completed,
                is_backgrounded: false,
                description: "demo".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                command: None,
                agent_type: None,
                server: None,
                tool: None,
                pending_tool_call: None,
                name: Some("audit".to_string()),
                workflow_run_id: Some("workflow-run-1".to_string()),
                phase_count: Some(2),
                workflow_progress: None,
                workflow_phases: Vec::new(),
                workflow_agents: Vec::new(),
                workflow_script_path: None,
                workflow_launch_input: None,
                workflow_final_summary: None,
                workflow_failure_count: 0,
                usage: None,
                subagent_current_activity: None,
                subagent_turn: None,
                last_activity_at_ms: None,
                result: None,
                error: None,
                retry_count: 0,
                output_truncated: false,
                publication_revision: None,
            }],
        });
        state.update(TuiEvent::WorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done".to_string(),
        });

        assert_eq!(state.workflow_panel.tasks.len(), 1);
        assert_eq!(state.workflow_panel.selected, 0);
        let notification = state
            .pending_workflow_notifications
            .pop_front()
            .expect("pending workflow notification");
        assert_eq!(notification.id, "notification-1");
        assert_eq!(
            notification.prompt,
            "<task-notification>done</task-notification>"
        );
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message)) if message.contains("Workflow completed. audit: done")
        ));
    }

    #[test]
    fn duplicate_workflow_notification_id_is_not_queued_twice() {
        let mut state = state();

        state.update(TuiEvent::WorkflowNotification {
            id: "workflow-run-1:task-1:tool-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done".to_string(),
        });
        state.update(TuiEvent::WorkflowNotification {
            id: "workflow-run-1:task-1:tool-1".to_string(),
            prompt: "<task-notification>done again</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done again".to_string(),
        });

        assert_eq!(state.pending_workflow_notifications.len(), 1);
        assert_eq!(
            state.pending_workflow_notifications[0].prompt,
            "<task-notification>done</task-notification>"
        );
        let workflow_messages = state
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    ChatMessage::System(text) if text.starts_with("Workflow completed.")
                )
            })
            .count();
        assert_eq!(workflow_messages, 1);
    }

    #[test]
    fn pending_workflow_notification_queue_owns_unique_drain_and_notification_pop() {
        let queue = PendingWorkflowNotificationQueue::new();
        assert!(queue.push_unique(PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>one</task-notification>".to_string(),
        }));
        assert!(!queue.push_unique(PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>duplicate</task-notification>".to_string(),
        }));

        let mut pending = VecDeque::new();
        queue.drain_into(&mut pending);
        assert!(queue.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "notification-1");

        assert!(queue.push_unique(PendingWorkflowNotification {
            id: "stale-notification".to_string(),
            prompt: "<task-notification>stale</task-notification>".to_string(),
        }));
        queue.clear();
        assert!(queue.is_empty());

        assert!(queue.push_unique(PendingWorkflowNotification {
            id: "notification-2".to_string(),
            prompt: "<task-notification>two</task-notification>".to_string(),
        }));
        assert_eq!(
            queue
                .pop_notification()
                .as_ref()
                .map(|notification| (notification.id.as_str(), notification.prompt.as_str())),
            Some((
                "notification-2",
                "<task-notification>two</task-notification>"
            ))
        );
        assert!(queue.pop_notification().is_none());
    }

    #[test]
    fn workflow_task_updates_sort_actionable_active_then_recent_terminal_tasks() {
        let mut state = state();
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);

        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);

        let mut approval = workflow_task_summary("task-approval", "approval");
        approval.task_type = TaskType::MainSession;
        approval.status = TaskStatus::ApprovalRequired;
        approval.is_backgrounded = true;
        approval.last_activity_at_ms = Some(1_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![completed, running, approval],
        });

        assert_eq!(
            state
                .workflow_panel
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-approval", "task-running", "task-completed"]
        );
    }

    #[test]
    fn workflow_task_updates_preserve_selected_task_id_after_sorting() {
        let mut state = state();
        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);
        state.workflow_panel.tasks = vec![running.clone(), completed.clone()];
        state.workflow_panel.selected = 1;

        running.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![completed, running],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-completed"
        );
    }

    #[test]
    fn single_task_status_updates_merge_without_dropping_other_panel_tasks() {
        let mut state = state();
        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![running.clone(), completed.clone()],
        });
        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-completed")
            .expect("completed task remains visible");

        running.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTaskUpdated { task: running });

        assert_eq!(
            state
                .workflow_panel
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-running", "task-completed"]
        );
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-completed"
        );
    }

    #[test]
    fn backgrounded_main_session_update_reveals_and_selects_task_panel_once() {
        let mut state = state();
        let mut backgrounded = workflow_task_summary("task-main", "backgrounded");
        backgrounded.task_type = TaskType::MainSession;
        backgrounded.status = TaskStatus::Running;
        backgrounded.is_backgrounded = true;
        backgrounded.last_activity_at_ms = Some(8_000);
        let mut workflow = workflow_task_summary("task-workflow", "workflow");
        workflow.status = TaskStatus::Running;
        workflow.last_activity_at_ms = Some(9_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow.clone(), backgrounded.clone()],
        });

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-main"
        );

        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-workflow")
            .expect("workflow task remains visible");
        backgrounded.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow, backgrounded],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-workflow"
        );
    }

    #[test]
    fn backgrounded_approval_update_reveals_and_selects_task_panel_once() {
        let mut state = state();
        let mut approval = workflow_task_summary("task-approval", "approval");
        approval.task_type = TaskType::MainSession;
        approval.status = TaskStatus::ApprovalRequired;
        approval.is_backgrounded = true;
        approval.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "approval-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        approval.last_activity_at_ms = Some(8_000);
        let mut workflow = workflow_task_summary("task-workflow", "workflow");
        workflow.status = TaskStatus::Running;
        workflow.last_activity_at_ms = Some(9_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow.clone(), approval.clone()],
        });

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-approval"
        );

        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-workflow")
            .expect("workflow task remains visible");
        approval.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow, approval],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-workflow"
        );
    }

    #[test]
    fn backgrounded_main_session_suppresses_foreground_output_until_completion() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-main".to_string(),
                task_type: TaskType::MainSession,
                status: TaskStatus::Running,
                is_backgrounded: true,
                description: "long answer".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: None,
                command: None,
                agent_type: Some("main-session".to_string()),
                server: None,
                tool: None,
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
                subagent_turn: None,
                last_activity_at_ms: None,
                result: None,
                error: None,
                retry_count: 0,
                output_truncated: false,
                publication_revision: None,
            }],
        });

        state.update(TuiEvent::MessageDelta(
            "hidden background output".to_string(),
        ));
        assert!(state.messages.is_empty());

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state.update(TuiEvent::TurnStarted {
            turn: 2,
            task: None,
        });
        state.update(TuiEvent::MessageDelta(
            "visible foreground output\n".to_string(),
        ));

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Assistant(text)) if text == "visible foreground output\n"
        ));
    }

    #[test]
    fn foregrounded_main_session_task_update_clears_output_suppression() {
        let mut state = state();
        state.suppress_background_main_session_output = true;

        let mut task = workflow_task_summary("task-main", "foregrounded");
        task.task_type = TaskType::MainSession;
        task.status = TaskStatus::Running;
        task.is_backgrounded = false;
        state.update(TuiEvent::WorkflowTasksUpdated { tasks: vec![task] });

        assert!(!state.suppress_background_main_session_output);
    }

    #[test]
    fn background_output_attach_clears_suppression_before_replayed_delta() {
        let mut state = state();
        state.panel_mode = PanelMode::Workflows;
        state.suppress_background_main_session_output = true;

        state.update(TuiEvent::BackgroundTaskOutputAttached {
            task_id: "task-main".to_string(),
        });
        state.update(TuiEvent::MessageDelta(
            "missing foreground suffix\n".to_string(),
        ));

        assert!(!state.suppress_background_main_session_output);
        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Assistant(text)) if text == "missing foreground suffix\n"
        ));
    }

    #[test]
    fn foregrounded_selected_main_session_returns_to_conversation_panel() {
        let mut state = state();
        state.panel_mode = PanelMode::Workflows;
        state.suppress_background_main_session_output = true;

        let mut selected = workflow_task_summary("task-main", "selected");
        selected.task_type = TaskType::MainSession;
        selected.status = TaskStatus::Running;
        selected.is_backgrounded = true;
        let mut other = workflow_task_summary("task-other", "other");
        other.status = TaskStatus::Running;
        state.workflow_panel.tasks = vec![selected.clone(), other.clone()];
        state.workflow_panel.selected = 0;

        selected.is_backgrounded = false;
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![selected, other],
        });

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(!state.suppress_background_main_session_output);
    }

    #[test]
    fn backgrounded_main_session_completion_adds_system_notice() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-main".to_string(),
                task_type: TaskType::MainSession,
                status: TaskStatus::Running,
                is_backgrounded: true,
                description: "long answer".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: None,
                command: None,
                agent_type: Some("main-session".to_string()),
                server: None,
                tool: None,
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
                subagent_turn: None,
                last_activity_at_ms: None,
                result: None,
                error: None,
                retry_count: 0,
                output_truncated: false,
                publication_revision: None,
            }],
        });

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message == "Background session completed: success"
        ));
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn active_goal_projection_does_not_mark_running_app_idle() {
        let mut state = state();
        state.status = AppStatus::Running;
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "keep going".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        assert_eq!(state.status, AppStatus::Running);

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(
            SurfaceProjectionState {
                cursor: crate::surface_projection::test_surface_cursor(1),
                session_id: Some("session-1".to_string()),
                title: "Goal session".to_string(),
                usage_revision: 1,
                usage: UsageTotals::default(),
                context_revision: 1,
                context_used_tokens: 0,
                context_limit_tokens: 128_000,
                workflow_tasks: Vec::new(),
                current_goal: Some(goal),
                foreground_operation_id: None,
                recoverable_operation_id: None,
                goal_presentation: Some(
                    crate::surface_projection::GoalProjectionPresentation::Updated,
                ),
                session_presentation: None,
            },
        )));
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn goal_status_is_presentation_only() {
        let mut state = state();
        let committed = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "committed objective".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Paused,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 2,
        };
        let queried = ThreadGoal {
            objective: "queried objective".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            updated_at: 3,
            ..committed.clone()
        };
        state.replace_current_goal_for_test(Some(committed.clone()));

        state.update(TuiEvent::GoalStatus(Some(queried)));

        assert_eq!(state.current_goal(), Some(&committed));
        assert!(state.messages.iter().any(
            |message| matches!(message, ChatMessage::System(text) if text.contains("queried objective"))
        ));
    }

    #[test]
    fn goal_status_messages_compact_long_objectives() {
        let mut state = state();
        let objective = "目标内容很长".repeat(100);
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: objective.clone(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: Some(2_000),
            tokens_used: 1_500,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal)));

        let Some(ChatMessage::System(message)) = state.messages.last() else {
            panic!("goal status should add a system message");
        };
        assert!(message.starts_with("Goal active · 目标内容"));
        assert!(message.contains('…'));
        assert!(message.ends_with("2m · 1.5K/2K tok"));
        assert!(!message.contains(&objective));
    }

    #[test]
    fn running_goal_does_not_repeat_unchanged_status_notice() {
        let mut state = state();
        state.status = AppStatus::Running;
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "keep going".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        state.update(TuiEvent::GoalStatus(Some(goal)));

        assert_eq!(
            state
                .messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::System(text) if text.starts_with("Goal active")))
                .count(),
            1
        );
    }

    #[test]
    fn idle_goal_refreshes_between_turns_do_not_repeat_unchanged_status_notice() {
        // Between auto-continuation turns the goal loop emits several GoalStatus
        // refreshes (pre-turn poll, usage accounting, post-turn poll) while the
        // app has already returned to Idle. They render an identical line, so the
        // transcript must collapse them to a single notice regardless of status.
        let mut state = state();
        state.status = AppStatus::Idle;
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "keep going".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        state.update(TuiEvent::GoalStatus(Some(goal)));

        assert_eq!(
            state
                .messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::System(text) if text.starts_with("Goal active")))
                .count(),
            1
        );
    }

    #[test]
    fn compacted_event_explains_runtime_recovery_reason() {
        let mut state = state();
        state.status = AppStatus::Compacting;

        state.update(TuiEvent::Compacted {
            before_messages: 12,
            after_messages: 5,
            reason: "prompt_too_long_recovery".to_string(),
            strategy: "remote_summary".to_string(),
            collapsed_messages: 7,
            status_text: "compacted context after prompt-too-long".to_string(),
        });

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message == "Compacted conversation context after prompt-too-long: 12 -> 5 messages (collapsed 7, remote_summary)."
        ));
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn compaction_lifecycle_sets_compacting_until_completion() {
        let mut state = state();

        state.update(TuiEvent::CompactionStarted);
        assert_eq!(state.status, AppStatus::Compacting);

        state.update(TuiEvent::Compacted {
            before_messages: 12,
            after_messages: 5,
            reason: "manual".to_string(),
            strategy: "manual".to_string(),
            collapsed_messages: 7,
            status_text: "compacted context manually".to_string(),
        });
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn running_timer_starts_and_stops_with_running_status() {
        let mut state = state();
        assert!(state.running_started_at.is_none());

        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        assert!(state.running_started_at.is_some());

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.running_started_at.is_none());
    }

    #[test]
    fn approval_round_trip_preserves_running_timer() {
        let mut state = state();
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let started_at = Instant::now() - std::time::Duration::from_secs(65);
        state.running_started_at = Some(started_at);

        state.update(TuiEvent::ApprovalNeeded {
            key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });
        assert_eq!(state.status, AppStatus::WaitingApproval);
        assert_eq!(state.running_started_at, Some(started_at));

        state.enter_running();
        assert_eq!(state.status, AppStatus::Running);
        assert_eq!(state.running_started_at, Some(started_at));
    }

    const EDIT_DIFF: &str = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

    fn configured_edit_state() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().expect("edit workspace");
        std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
        std::fs::write(directory.path().join("src/item.py"), "value = 2\n")
            .expect("post-edit file");
        let mut state = state();
        state.configure_syntax_highlighting(
            directory.path().to_path_buf(),
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        );
        (directory, state)
    }

    fn submit_live_edit(state: &mut AppState, id: &str, target: &str, diff: &str) {
        state.update(TuiEvent::ToolRequested {
            id: id.to_string(),
            name: "edit".to_string(),
            target: Some(target.to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: id.to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: format!("edited {target}"),
            diff: Some(diff.to_string()),
            kind: None,
        });
    }

    fn malformed_structural_diffs() -> Vec<(&'static str, String)> {
        vec![
            (
                "malformed-hunk-candidate",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ malformed coordinates @@
@@ -1 +1 @@ valid function context
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "metadata-before-first-hunk",
                "\
--- a/src/item.py
+++ b/src/item.py
arbitrary metadata
@@ -1 +1 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "zero-old-start",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -0 +1 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "zero-new-start",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +0 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "zero-width",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1,0 @@
@@ -1 +1 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "overflow",
                format!(
                    "--- a/src/item.py\n+++ b/src/item.py\n@@ -{},2 +1,2 @@\n-old = 1\n-old = 2\n+value = 2\n+new = 2\n",
                    usize::MAX
                ),
            ),
            (
                "duplicate",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
@@ -1 +1 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "backward",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-old = 3
+new = 3
@@ -1 +1 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "old-overlap",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1 @@
-old = 1
-old = 2
+new = 1
@@ -2 +2 @@
-old = 2
+value = 2
"
                .to_string(),
            ),
            (
                "new-overlap",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,2 @@
-old = 1
+new = 1
+value = 2
@@ -2 +2 @@
-old = 2
+value = 2
"
                .to_string(),
            ),
            (
                "reused-old-anchor",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1 @@
+first = 1
@@ -1 +3 @@
-value = 1
+value = 2
"
                .to_string(),
            ),
            (
                "reused-new-anchor",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,0 @@
-value = 1
@@ -3 +1 @@
-other = 3
+value = 2
"
                .to_string(),
            ),
            (
                "null-old-range",
                "\
--- /dev/null
+++ b/src/item.py
@@ -1,0 +1 @@
+value = 2
"
                .to_string(),
            ),
            (
                "null-new-range",
                "\
--- a/src/item.py
+++ /dev/null
@@ -1 +1,0 @@
-value = 1
"
                .to_string(),
            ),
            (
                "both-null",
                "\
--- /dev/null
+++ /dev/null
@@ -0,0 +1 @@
+value = 2
"
                .to_string(),
            ),
        ]
    }

    fn state_with_submitted_edit_job() -> (
        tempfile::TempDir,
        AppState,
        crate::edit_highlight_worker::EditHighlightJob,
    ) {
        let (directory, mut state) = configured_edit_state();
        submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
        let job = state
            .pending_edit_highlight_job("edit-1")
            .expect("pending edit highlight job");
        (directory, state, job)
    }

    fn ready_result(
        job: crate::edit_highlight_worker::EditHighlightJob,
    ) -> crate::edit_highlight_worker::EditHighlightResult {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;

        let styles = crate::diff_highlight::RefinedDiffStyles::from([(
            1,
            vec![Span::styled(
                "value = 2".to_string(),
                Style::default().fg(Color::Magenta),
            )],
        )]);
        crate::edit_highlight_worker::EditHighlightResult {
            job,
            outcome: crate::edit_highlight_worker::EditHighlightOutcome::Ready {
                styles: Arc::new(styles),
            },
        }
    }

    #[cfg(unix)]
    fn real_alias_edit_state() -> (
        tempfile::TempDir,
        AppState,
        crate::edit_highlight_worker::EditHighlightJob,
    ) {
        use std::os::unix::fs::symlink;

        let (directory, mut state) = configured_edit_state();
        let alias = directory.path().join("src/alias.py");
        symlink(directory.path().join("src/item.py"), &alias).expect("initial alias");
        let request = orca_core::tool_types::ToolRequest {
            id: "alias-edit".to_string(),
            name: orca_core::tool_types::ToolName::Edit,
            action: orca_core::approval_types::ActionKind::Write,
            target: Some("src/alias.py".to_string()),
            raw_arguments: Some(
                r#"{"path":"src/alias.py","old_text":"value = 2","new_text":"value = 3"}"#
                    .to_string(),
            ),
        };
        let result = orca_tools::edit::execute(&request, directory.path());
        assert_eq!(
            result.status,
            orca_core::tool_types::ToolStatus::Completed,
            "symlink alias edit failed: {:?}",
            result.error
        );
        let preview = result
            .file_change_preview
            .as_deref()
            .expect("committed alias preview");
        let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview
        else {
            panic!("alias edit should produce unified diff");
        };
        state.update(TuiEvent::ToolRequested {
            id: request.id.clone(),
            name: "edit".to_string(),
            target: request.target.clone(),
        });
        state.update(TuiEvent::ToolCompleted {
            id: request.id,
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: result.output.unwrap_or_default(),
            diff: Some(diff.clone()),
            kind: None,
        });
        let job = state
            .pending_edit_highlight_job("alias-edit")
            .expect("real alias edit pending job");
        (directory, state, job)
    }

    #[cfg(unix)]
    #[test]
    fn real_edit_producer_keeps_symlink_alias_as_job_display_path() {
        let (directory, state, job) = real_alias_edit_state();

        assert_eq!(
            job.absolute_path,
            directory
                .path()
                .join("src/item.py")
                .canonicalize()
                .expect("canonical item path")
        );
        assert_eq!(job.display_path, "src/alias.py");
        assert_eq!(job.parsed.destination_path.as_deref(), Some("src/alias.py"));
        assert_eq!(state.pending_edit_highlight_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn ready_result_applies_while_symlink_alias_identity_is_unchanged() {
        let (_directory, mut state, job) = real_alias_edit_state();

        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_some()
        );
    }

    #[test]
    fn successful_live_edit_submits_one_versioned_highlight_job() {
        let (directory, mut state) = configured_edit_state();

        state.update(TuiEvent::ToolRequested {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited src/item.py".to_string(),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
        });

        let job = state
            .pending_edit_highlight_job("edit-1")
            .expect("pending job");
        assert!(state.edit_highlight_needs_tick());
        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert_eq!(state.successful_edit_highlight_submit_count(), 1);
        assert_eq!(job.tool_id, "edit-1");
        assert_eq!(job.message_index, 0);
        assert_eq!(job.message_revision, state.message_revisions[0]);
        assert_eq!(
            job.syntax_theme_revision,
            crate::terminal_capabilities::syntax_style_revision(
                crate::syntax_highlight::SyntaxTheme::OneHalfDark,
                crate::terminal_capabilities::TerminalColorLevel::TrueColor,
            )
        );
        assert_eq!(
            job.syntax_theme,
            crate::syntax_highlight::SyntaxTheme::OneHalfDark
        );
        assert_eq!(
            job.syntax_color_level,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor
        );
        assert_eq!(
            job.absolute_path,
            directory
                .path()
                .join("src/item.py")
                .canonicalize()
                .expect("canonical target")
        );
        assert_eq!(job.display_path, "src/item.py");
        assert_eq!(
            job.parsed,
            crate::diff_highlight::parse_unified_diff(EDIT_DIFF)
        );
    }

    #[test]
    fn completion_only_tool_row_has_no_target_and_submits_no_job() {
        let (_directory, mut state) = configured_edit_state();

        state.update(TuiEvent::ToolCompleted {
            id: "edit-1".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited src/item.py".to_string(),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
        });

        assert!(matches!(
            state.messages.first(),
            Some(ChatMessage::ToolCall { target: None, .. })
        ));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn replayed_history_messages_never_submit_jobs() {
        let (_directory, mut state) = configured_edit_state();
        let historical = ChatMessage::ToolCall {
            id: "historical-edit".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
            status: "completed".to_string(),
            output: None,
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
            expanded: false,
        };

        state.push_message(historical.clone());
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());

        state.replace_messages([historical]);
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn ineligible_live_edits_do_not_start_runtime_or_submit_jobs() {
        fn assert_ineligible(
            configure: impl FnOnce(&tempfile::TempDir, &mut AppState) -> (String, String, String),
        ) {
            let (directory, mut state) = configured_edit_state();
            let (status, target, diff) = configure(&directory, &mut state);
            state.update(TuiEvent::ToolRequested {
                id: "edit-ineligible".to_string(),
                name: "edit".to_string(),
                target: Some(target),
            });
            state.update(TuiEvent::ToolCompleted {
                id: "edit-ineligible".to_string(),
                name: "edit".to_string(),
                status,
                output: String::new(),
                diff: Some(diff),
                kind: None,
            });
            assert_eq!(state.pending_edit_highlight_count(), 0);
            assert!(!state.edit_highlight_runtime_started());
        }

        assert_ineligible(|_, _| {
            (
                "failed".to_string(),
                "src/item.py".to_string(),
                EDIT_DIFF.into(),
            )
        });
        assert_ineligible(|_, _| {
            (
                "cancelled".to_string(),
                "src/item.py".to_string(),
                EDIT_DIFF.into(),
            )
        });
        assert_ineligible(|_, _| {
            (
                "completed".to_string(),
                "src/item.py".to_string(),
                " \n".to_string(),
            )
        });
        assert_ineligible(|directory, _| {
            std::fs::write(directory.path().join("src/item.unknown"), "value = 2\n")
                .expect("unknown syntax file");
            (
                "completed".to_string(),
                "src/item.unknown".to_string(),
                EDIT_DIFF.replace("item.py", "item.unknown"),
            )
        });
        assert_ineligible(|_, _| {
            (
                "completed".to_string(),
                "src/item.py".to_string(),
                format!("{EDIT_DIFF}--- a/src/other.py\n+++ b/src/other.py\n@@ -1 +1 @@\n-a\n+b\n"),
            )
        });
        assert_ineligible(|directory, _| {
            std::fs::write(directory.path().join("src/item.py"), "").expect("empty post-edit file");
            (
                "completed".to_string(),
                "src/item.py".to_string(),
                "--- a/src/item.py\n+++ b/src/item.py\n@@ -1 +0,0 @@\n-value = 1\n".to_string(),
            )
        });
        assert_ineligible(|directory, _| {
            let outside = directory.path().parent().unwrap().join("outside-item.py");
            std::fs::write(&outside, "value = 2\n").expect("outside file");
            (
                "completed".to_string(),
                "../outside-item.py".to_string(),
                EDIT_DIFF.replace("src/item.py", "../outside-item.py"),
            )
        });
        assert_ineligible(|directory, _| {
            std::fs::remove_file(directory.path().join("src/item.py")).expect("remove source file");
            std::fs::create_dir(directory.path().join("src/item.py"))
                .expect("file-shaped directory");
            (
                "completed".to_string(),
                "src/item.py".to_string(),
                EDIT_DIFF.into(),
            )
        });
    }

    #[test]
    fn live_edit_without_configured_workspace_submits_no_job() {
        let mut state = state();

        submit_live_edit(&mut state, "no-workspace", "src/item.py", EDIT_DIFF);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn incomplete_unified_diff_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let incomplete = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "incomplete", "src/item.py", incomplete);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn malformed_structures_fail_closed_across_parser_first_paint_and_app_state() {
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);

        for (id, diff) in malformed_structural_diffs() {
            let parsed = crate::diff_highlight::parse_unified_diff(&diff);
            assert!(parsed.has_malformed_hunk, "{id}");
            assert!(!parsed.is_structurally_valid(), "{id}");
            assert_eq!(parsed.raw_fallback.as_deref(), Some(diff.as_str()), "{id}");

            let rendered = crate::diff_highlight::render_parsed_diff(&parsed, &theme, None);
            assert_eq!(rendered.len(), diff.lines().count(), "{id}");
            for (raw_line, rendered_line) in diff.lines().zip(rendered) {
                assert_eq!(
                    rendered_line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                    format!("    {raw_line}"),
                    "{id}"
                );
                assert_eq!(rendered_line.spans.len(), 1, "{id}: {raw_line:?}");
            }

            let (_directory, mut state) = configured_edit_state();
            submit_live_edit(&mut state, id, "src/item.py", &diff);
            assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
            assert!(!state.edit_highlight_runtime_started(), "{id}");
        }
    }

    #[test]
    fn headerless_then_headered_diff_is_ambiguous_and_submits_no_job() {
        let (_directory, mut state) = configured_edit_state();
        let diff = "\
@@ -1 +1 @@
-value = 1
+value = 2
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-other = 3
+other = 4
";

        let parsed = crate::diff_highlight::parse_unified_diff(diff);
        assert!(parsed.is_structurally_valid());
        assert!(parsed.has_multiple_files);

        submit_live_edit(&mut state, "mixed-sections", "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn extra_source_line_after_completed_hunk_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let malformed = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
+unexpected = 3
";

        submit_live_edit(&mut state, "malformed", "src/item.py", malformed);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn missing_file_header_pair_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let malformed = "\
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "missing-header", "src/item.py", malformed);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn valid_rename_diff_uses_destination_target_and_submits_job() {
        let (_directory, mut state) = configured_edit_state();
        let renamed = "\
--- a/src/old.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "rename", "src/item.py", renamed);

        let job = state
            .pending_edit_highlight_job("rename")
            .expect("rename destination job");
        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert_eq!(job.display_path, "src/item.py");
    }

    #[test]
    fn valid_added_file_diff_uses_destination_target_and_submits_job() {
        let (_directory, mut state) = configured_edit_state();
        let added = "\
--- /dev/null
+++ b/src/item.py
@@ -0,0 +1 @@
+value = 2
";

        submit_live_edit(&mut state, "add", "src/item.py", added);

        let job = state
            .pending_edit_highlight_job("add")
            .expect("added file destination job");
        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert_eq!(job.display_path, "src/item.py");
    }

    #[test]
    fn dev_null_requires_zero_start_and_zero_count() {
        let (_directory, mut state) = configured_edit_state();
        let invalid_add = "\
--- /dev/null
+++ b/src/item.py
@@ -1,0 +1 @@
+value = 2
";
        let invalid_delete = "\
--- a/src/item.py
+++ /dev/null
@@ -1 +1,0 @@
-value = 1
";

        submit_live_edit(&mut state, "invalid-null-add", "src/item.py", invalid_add);
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!parsed_diff_structure_matches_target(
            &crate::diff_highlight::parse_unified_diff(invalid_add),
            invalid_add,
            Path::new("src/item.py")
        ));
        assert!(!parsed_diff_structure_matches_target(
            &crate::diff_highlight::parse_unified_diff(invalid_delete),
            invalid_delete,
            Path::new("src/item.py")
        ));
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn in_workspace_parent_component_normalizes_and_submits_job() {
        let (directory, mut state) = configured_edit_state();
        std::fs::write(directory.path().join("item.py"), "value = 2\n")
            .expect("normalized post-edit file");
        let diff = "\
--- a/item.py
+++ b/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "normalized", "src/../item.py", diff);

        let job = state
            .pending_edit_highlight_job("normalized")
            .expect("normalized target job");
        assert_eq!(job.display_path, "item.py");
        assert_eq!(
            job.absolute_path,
            directory
                .path()
                .join("item.py")
                .canonicalize()
                .expect("canonical normalized target")
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_edit_producer_lexically_normalizes_symlink_parent_target() {
        use std::os::unix::fs::symlink;

        let (directory, mut state) = configured_edit_state();
        let outside = tempfile::tempdir().expect("outside root");
        symlink(outside.path(), directory.path().join("link")).expect("outside symlink");
        let request = orca_core::tool_types::ToolRequest {
            id: "parent-edit".to_string(),
            name: orca_core::tool_types::ToolName::Edit,
            action: orca_core::approval_types::ActionKind::Write,
            target: Some("link/../src/item.py".to_string()),
            raw_arguments: Some(
                r#"{"path":"link/../src/item.py","old_text":"value = 2","new_text":"value = 3"}"#
                    .to_string(),
            ),
        };
        let result = orca_tools::edit::execute(&request, directory.path());
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
        let preview = result
            .file_change_preview
            .as_deref()
            .expect("committed parent preview");
        let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview
        else {
            panic!("parent edit should produce unified diff");
        };
        state.update(TuiEvent::ToolRequested {
            id: request.id.clone(),
            name: "edit".to_string(),
            target: request.target.clone(),
        });
        state.update(TuiEvent::ToolCompleted {
            id: request.id,
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: result.output.unwrap_or_default(),
            diff: Some(diff.clone()),
            kind: None,
        });

        let job = state
            .pending_edit_highlight_job("parent-edit")
            .expect("parent edit pending job");
        assert_eq!(
            job.absolute_path,
            directory
                .path()
                .join("src/item.py")
                .canonicalize()
                .expect("canonical item")
        );
        assert_eq!(job.display_path, "src/item.py");
        assert_eq!(job.parsed.destination_path.as_deref(), Some("src/item.py"));
    }

    #[test]
    fn real_edit_producer_allows_parent_reentry_into_same_workspace() {
        let (directory, mut state) = configured_edit_state();
        let workspace_name = directory
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 workspace name");
        let target = format!("../{workspace_name}/src/item.py");
        let request = orca_core::tool_types::ToolRequest {
            id: "parent-reentry-edit".to_string(),
            name: orca_core::tool_types::ToolName::Edit,
            action: orca_core::approval_types::ActionKind::Write,
            target: Some(target.clone()),
            raw_arguments: Some(format!(
                r#"{{"path":"{target}","old_text":"value = 2","new_text":"value = 3"}}"#
            )),
        };
        let result = orca_tools::edit::execute(&request, directory.path());
        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
        let preview = result
            .file_change_preview
            .as_deref()
            .expect("committed parent-reentry preview");
        let orca_core::tool_types::FileChangePreview::UnifiedDiff { text: diff, .. } = preview
        else {
            panic!("parent-reentry edit should produce unified diff");
        };
        let parsed = crate::diff_highlight::parse_unified_diff(diff);
        let expected_relative = PathBuf::from("src")
            .join("item.py")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            parsed.destination_path.as_deref(),
            Some(expected_relative.as_str())
        );
        state.update(TuiEvent::ToolRequested {
            id: request.id.clone(),
            name: "edit".to_string(),
            target: request.target.clone(),
        });
        state.update(TuiEvent::ToolCompleted {
            id: request.id,
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: result.output.unwrap_or_default(),
            diff: Some(diff.clone()),
            kind: None,
        });

        let job = state
            .pending_edit_highlight_job("parent-reentry-edit")
            .expect("parent-reentry pending job");
        assert_eq!(
            job.absolute_path,
            directory
                .path()
                .join("src/item.py")
                .canonicalize()
                .expect("canonical item")
        );
        assert_eq!(job.display_path, "src/item.py");
        assert_eq!(
            job.parsed.destination_path.as_deref(),
            Some(expected_relative.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_target_resolution_matches_tool_resolution_table() {
        use std::os::unix::fs::symlink;

        let (directory, state) = configured_edit_state();
        std::fs::write(directory.path().join("item.py"), "value = 2\n").expect("root item");
        symlink(
            directory.path().join("src/item.py"),
            directory.path().join("src/alias.py"),
        )
        .expect("alias symlink");
        let outside = tempfile::tempdir().expect("outside root");
        std::fs::create_dir(outside.path().join("child")).expect("outside child");
        std::fs::write(outside.path().join("escaped.py"), "value = 2\n").expect("outside file");
        symlink(
            outside.path().join("child"),
            directory.path().join("linked"),
        )
        .expect("outside child symlink");

        let workspace_name = directory
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 workspace name");
        let parent_reentry = format!("../{workspace_name}/src/item.py");
        let cases = vec![
            ("src/item.py".to_string(), Some("src/item.py")),
            ("src/../item.py".to_string(), Some("item.py")),
            ("src/alias.py".to_string(), Some("src/alias.py")),
            (parent_reentry, Some("src/item.py")),
            ("../escaped.py".to_string(), None),
            ("linked/escaped.py".to_string(), None),
        ];

        for (target, expected_display) in cases {
            let tool_path = orca_tools::resolve_workspace_path(directory.path(), Some(&target))
                .ok()
                .filter(|path| path.is_file())
                .and_then(|path| path.canonicalize().ok());
            let app_path = state.resolve_edit_target(&target);
            assert_eq!(app_path.is_some(), tool_path.is_some(), "{target}");
            if let (Some((app_absolute, display)), Some(tool_absolute)) = (app_path, tool_path) {
                assert_eq!(app_absolute, tool_absolute, "{target}");
                assert_eq!(
                    display,
                    expected_display.expect("expected display"),
                    "{target}"
                );
            }
        }
    }

    #[test]
    fn reversed_file_headers_are_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let reversed = "\
+++ b/src/item.py
--- a/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "reversed", "src/item.py", reversed);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn arbitrary_in_hunk_and_trailing_metadata_are_rejected() {
        for (id, diff) in [
            (
                "leading-metadata",
                "\
arbitrary metadata
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
",
            ),
            (
                "in-hunk-metadata",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@
-value = 1
+value = 2
arbitrary metadata
 shared = 3
",
            ),
            (
                "trailing-metadata",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-value = 1
+value = 2
arbitrary metadata
",
            ),
        ] {
            let (_directory, mut state) = configured_edit_state();

            submit_live_edit(&mut state, id, "src/item.py", diff);

            assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
            assert!(!state.edit_highlight_runtime_started(), "{id}");
        }
    }

    #[test]
    fn standard_no_newline_metadata_and_header_timestamps_are_allowed() {
        let (_directory, mut state) = configured_edit_state();
        let diff = "\
--- a/src/item.py\t2026-07-24 10:00:00
+++ b/src/item.py\t2026-07-24 10:01:00
@@ -1 +1 @@
-value = 1
\\ No newline at end of file
+value = 2
\\ No newline at end of file
";

        submit_live_edit(&mut state, "standard-metadata", "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert!(
            state
                .pending_edit_highlight_job("standard-metadata")
                .is_some()
        );
    }

    #[test]
    fn zero_new_side_coordinate_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let invalid = "\
--- a/src/item.py
+++ b/src/item.py
@@ -0,0 +0,1 @@
+value = 2
";

        submit_live_edit(&mut state, "zero-new-line", "src/item.py", invalid);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn positive_count_with_zero_start_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let invalid = "\
--- a/src/item.py
+++ b/src/item.py
@@ -0 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "zero-old-start", "src/item.py", invalid);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn positive_start_zero_count_mid_file_insertion_is_eligible() {
        let (directory, mut state) = configured_edit_state();
        std::fs::write(
            directory.path().join("src/item.py"),
            "first = 1\nvalue = 2\n",
        )
        .expect("post-insert file");
        let insertion = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +2 @@ fn context
+value = 2
";

        submit_live_edit(&mut state, "mid-insert", "src/item.py", insertion);

        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert!(state.pending_edit_highlight_job("mid-insert").is_some());
    }

    #[test]
    fn empty_zero_width_hunk_is_rejected_before_runtime_spawn() {
        let (_directory, mut state) = configured_edit_state();
        let diff = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,0 +1,0 @@
@@ -1 +1 @@
-value = 1
+value = 2
";

        submit_live_edit(&mut state, "empty-hunk", "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn duplicate_backward_and_overlapping_hunks_are_rejected() {
        let cases = [
            (
                "duplicate-hunk",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1 @@
-old = 1
+value = 2
@@ -1 +1 @@
-old = 1
+value = 2
",
            ),
            (
                "backward-hunk",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -3 +3 @@
-old = 3
+new = 3
@@ -1 +1 @@
-old = 1
+value = 2
",
            ),
            (
                "old-overlap",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1 @@
-old = 1
-old = 2
+new = 1
@@ -2 +2 @@
-old = 2
+value = 2
",
            ),
            (
                "new-overlap",
                "\
--- a/src/item.py
+++ b/src/item.py
@@ -1 +1,2 @@
-old = 1
+new = 1
+value = 2
@@ -2 +2 @@
-old = 2
+value = 2
",
            ),
        ];

        for (id, diff) in cases {
            let (_directory, mut state) = configured_edit_state();

            submit_live_edit(&mut state, id, "src/item.py", diff);

            assert_eq!(state.pending_edit_highlight_count(), 0, "{id}");
            assert!(!state.edit_highlight_runtime_started(), "{id}");
        }
    }

    #[test]
    fn overflowing_hunk_endpoint_is_rejected() {
        let (_directory, mut state) = configured_edit_state();
        let diff = format!(
            "--- a/src/item.py\n+++ b/src/item.py\n@@ -{},2 +1,2 @@\n-old = 1\n-old = 2\n+value = 2\n+new = 2\n",
            usize::MAX
        );

        submit_live_edit(&mut state, "overflowing-hunk", "src/item.py", &diff);

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn two_non_overlapping_hunks_with_function_context_are_eligible() {
        let (directory, mut state) = configured_edit_state();
        std::fs::write(
            directory.path().join("src/item.py"),
            "first = 1\nvalue = 2\nthird = 3\nvalue = 4\n",
        )
        .expect("two-hunk post-edit file");
        let diff = "\
--- a/src/item.py
+++ b/src/item.py
@@ -1,2 +1,2 @@ first section
 first = 1
-old = 1
+value = 2
@@ -3,2 +3,2 @@ second section
 third = 3
-old = 2
+value = 4
";

        submit_live_edit(&mut state, "two-hunks", "src/item.py", diff);

        assert_eq!(state.pending_edit_highlight_count(), 1);
        assert!(state.pending_edit_highlight_job("two-hunks").is_some());
    }

    #[test]
    fn dev_null_counts_correlate_and_delete_only_stays_ineligible() {
        let (_directory, mut state) = configured_edit_state();
        let malformed_add = "\
--- /dev/null
+++ b/src/item.py
@@ -1 +1 @@
-old
+value = 2
";
        let delete = "\
--- a/src/item.py
+++ /dev/null
@@ -1 +0,0 @@
-value = 1
";

        submit_live_edit(&mut state, "malformed-add", "src/item.py", malformed_add);
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!parsed_diff_structure_matches_target(
            &crate::diff_highlight::parse_unified_diff(malformed_add),
            malformed_add,
            Path::new("src/item.py")
        ));

        assert!(parsed_diff_structure_matches_target(
            &crate::diff_highlight::parse_unified_diff(delete),
            delete,
            Path::new("src/item.py")
        ));
        submit_live_edit(&mut state, "delete", "src/item.py", delete);
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn absolute_empty_mismatch_and_symlink_escape_targets_are_rejected() {
        let (directory, mut state) = configured_edit_state();
        let absolute = directory.path().join("src/item.py");
        submit_live_edit(
            &mut state,
            "absolute",
            absolute.to_str().expect("utf-8 absolute path"),
            EDIT_DIFF,
        );
        assert!(state.pending_edit_highlight_job("absolute").is_none());
        assert!(!state.edit_highlight_runtime_started());

        submit_live_edit(&mut state, "empty", "", EDIT_DIFF);
        assert!(state.pending_edit_highlight_job("empty").is_none());
        assert!(!state.edit_highlight_runtime_started());

        submit_live_edit(
            &mut state,
            "mismatch",
            "src/item.py",
            &EDIT_DIFF.replace("src/item.py", "src/other.py"),
        );
        assert!(state.pending_edit_highlight_job("mismatch").is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let alias_parent = tempfile::tempdir().expect("workspace alias parent");
            let workspace_alias = alias_parent.path().join("workspace");
            symlink(directory.path(), &workspace_alias).expect("workspace symlink");
            state.configure_syntax_highlighting(
                workspace_alias,
                crate::syntax_highlight::SyntaxTheme::OneHalfDark,
                crate::terminal_capabilities::TerminalColorLevel::TrueColor,
            );
            let outside = tempfile::tempdir().expect("outside directory");
            std::fs::write(outside.path().join("escaped.py"), "value = 2\n").expect("outside file");
            symlink(outside.path(), directory.path().join("linked")).expect("outside symlink");
            submit_live_edit(
                &mut state,
                "symlink-ancestor",
                "linked/escaped.py",
                &EDIT_DIFF.replace("src/item.py", "linked/escaped.py"),
            );
            assert!(!state.edit_highlight_runtime_started());
        }
    }

    #[test]
    fn targetless_tool_request_and_completion_submit_no_job() {
        let (_directory, mut state) = configured_edit_state();
        state.update(TuiEvent::ToolRequested {
            id: "targetless".to_string(),
            name: "edit".to_string(),
            target: None,
        });
        state.update(TuiEvent::ToolCompleted {
            id: "targetless".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited".to_string(),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
        });

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn completion_with_only_settled_reused_id_pushes_targetless_row() {
        for old_status in ["completed", "failed"] {
            let (_directory, mut state) = configured_edit_state();
            state.push_message(ChatMessage::ToolCall {
                id: "reused".to_string(),
                name: "edit".to_string(),
                target: Some("src/item.py".to_string()),
                status: old_status.to_string(),
                output: Some("old output".to_string()),
                diff: Some(EDIT_DIFF.to_string()),
                kind: None,
                expanded: false,
            });

            state.update(TuiEvent::ToolCompleted {
                id: "reused".to_string(),
                name: "edit".to_string(),
                status: "completed".to_string(),
                output: "new output".to_string(),
                diff: Some(EDIT_DIFF.to_string()),
                kind: None,
            });

            assert_eq!(state.messages.len(), 2, "{old_status}");
            assert!(matches!(
                &state.messages[0],
                ChatMessage::ToolCall {
                    target: Some(target),
                    status,
                    output: Some(output),
                    ..
                } if target == "src/item.py" && status == old_status && output == "old output"
            ));
            assert!(matches!(
                &state.messages[1],
                ChatMessage::ToolCall {
                    target: None,
                    status,
                    output: Some(output),
                    ..
                } if status == "completed" && output == "new output"
            ));
            assert_eq!(state.pending_edit_highlight_count(), 0);
            assert!(!state.edit_highlight_runtime_started());
        }
    }

    #[test]
    fn injected_worker_spawn_failure_is_silent_and_leaves_no_pending_state() {
        fn fail_runtime() -> std::io::Result<crate::edit_highlight_worker::EditHighlightRuntime> {
            Err(std::io::Error::other("injected spawn failure"))
        }

        let (_directory, mut state) = configured_edit_state();
        state.set_edit_highlight_runtime_factory_for_test(fail_runtime);
        submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);

        assert_eq!(state.messages.len(), 1);
        assert!(matches!(
            state.messages.first(),
            Some(ChatMessage::ToolCall { id, .. }) if id == "edit-1"
        ));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }

    #[test]
    fn exact_ready_result_touches_only_matching_message_and_stores_arc_map() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        state.push_message(ChatMessage::System("unrelated".to_string()));
        let revisions_before = state.message_revisions.clone();
        let result = ready_result(job.clone());
        let expected_styles = match &result.outcome {
            crate::edit_highlight_worker::EditHighlightOutcome::Ready { styles } => {
                Arc::clone(styles)
            }
            crate::edit_highlight_worker::EditHighlightOutcome::Failed => unreachable!(),
        };

        assert!(state.apply_edit_highlight_result(result));
        assert_ne!(
            state.message_revisions[job.message_index],
            revisions_before[job.message_index]
        );
        assert_eq!(
            state.message_revisions[job.message_index + 1],
            revisions_before[job.message_index + 1]
        );
        assert!(Arc::ptr_eq(
            state
                .edit_highlights
                .applied()
                .get(&state.message_revisions[job.message_index])
                .map(|highlight| &highlight.styles)
                .expect("applied styles"),
            &expected_styles
        ));
        assert_eq!(
            state.edit_highlights.applied()[&state.message_revisions[job.message_index]].tool_id,
            job.tool_id
        );
        assert_eq!(
            state.edit_highlights.applied()[&state.message_revisions[job.message_index]]
                .display_path,
            job.display_path
        );
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_some()
        );
        assert_eq!(state.pending_edit_highlight_count(), 0);
    }

    #[test]
    fn refinement_rebuilds_only_matching_message_then_steady_and_scroll_build_nothing() {
        use std::cell::RefCell;

        let (_directory, mut state, job) = state_with_submitted_edit_job();
        state.push_message(ChatMessage::System("stable".to_string()));
        let stable_index = job.message_index + 1;
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let built_indices = RefCell::new(Vec::new());

        {
            let messages = &state.messages;
            let revisions = &state.message_revisions;
            let highlights = state.edit_highlights.applied();
            let cache = &mut state.transcript_render_cache;
            cache.prepare(
                messages,
                revisions,
                TranscriptRenderContext::new(&theme, 80, 0, false),
                |index, message, theme, width, tick, force_expand| {
                    built_indices.borrow_mut().push(index);
                    let refined = AppState::refined_diff_styles_for_message(
                        revisions, highlights, index, message,
                    );
                    crate::ui::build_lines_for_message(
                        message,
                        theme,
                        width,
                        tick,
                        force_expand,
                        refined,
                    )
                },
            );
        }
        assert_eq!(
            *built_indices.borrow(),
            vec![job.message_index, stable_index]
        );
        let revisions_before = state.message_revisions.clone();
        built_indices.borrow_mut().clear();

        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        {
            let messages = &state.messages;
            let revisions = &state.message_revisions;
            let highlights = state.edit_highlights.applied();
            let cache = &mut state.transcript_render_cache;
            cache.prepare(
                messages,
                revisions,
                TranscriptRenderContext::new(&theme, 80, 0, false),
                |index, message, theme, width, tick, force_expand| {
                    built_indices.borrow_mut().push(index);
                    let refined = AppState::refined_diff_styles_for_message(
                        revisions, highlights, index, message,
                    );
                    crate::ui::build_lines_for_message(
                        message,
                        theme,
                        width,
                        tick,
                        force_expand,
                        refined,
                    )
                },
            );
        }

        assert_eq!(*built_indices.borrow(), vec![job.message_index]);
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 1);
        assert_eq!(
            state.message_revisions[stable_index],
            revisions_before[stable_index]
        );
        let viewport = state
            .transcript_render_cache
            .viewport(0, usize::MAX, usize::MAX);
        let inserted = inserted_source_line(&viewport.lines, "value = 2");
        assert!(
            inserted
                .spans
                .iter()
                .any(|span| { span.style.fg == Some(ratatui::style::Color::Magenta) })
        );

        built_indices.borrow_mut().clear();
        {
            let messages = &state.messages;
            let revisions = &state.message_revisions;
            let highlights = state.edit_highlights.applied();
            let cache = &mut state.transcript_render_cache;
            cache.prepare(
                messages,
                revisions,
                TranscriptRenderContext::new(&theme, 80, 0, false),
                |index, message, theme, width, tick, force_expand| {
                    built_indices.borrow_mut().push(index);
                    let refined = AppState::refined_diff_styles_for_message(
                        revisions, highlights, index, message,
                    );
                    crate::ui::build_lines_for_message(
                        message,
                        theme,
                        width,
                        tick,
                        force_expand,
                        refined,
                    )
                },
            );
        }
        assert!(built_indices.borrow().is_empty());
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 0);

        let _ = state.transcript_render_cache.viewport(0, 0, 1);
        let _ = state
            .transcript_render_cache
            .viewport(0, usize::MAX, usize::MAX);
        assert!(built_indices.borrow().is_empty());
        assert_eq!(state.transcript_render_cache.last_prepare_visited(), 0);
    }

    #[test]
    fn real_worker_result_becomes_exact_message_styles_and_warms_rendering() {
        const SCOPED_DIFF: &str = "\
--- a/item.py
+++ b/item.py
@@ -3,2 +3,2 @@
     \"\"\"
-    field = 0
+    field = 1
";
        let directory = tempfile::tempdir().expect("scoped edit workspace");
        std::fs::write(
            directory.path().join("item.py"),
            "\
class Item:
    \"\"\"Summary.
    \"\"\"
    field = 1
",
        )
        .expect("post-edit Python file");
        let mut state = state();
        state.configure_syntax_highlighting(
            directory.path().to_path_buf(),
            crate::syntax_highlight::SyntaxTheme::OneHalfDark,
            crate::terminal_capabilities::TerminalColorLevel::TrueColor,
        );
        submit_live_edit(&mut state, "scoped-edit", "item.py", SCOPED_DIFF);
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let cold =
            crate::ui::build_lines_for_message(&state.messages[0], &theme, 80, 0, false, None);

        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if state.poll_edit_highlight_results() {
                break;
            }
            assert!(
                state.edit_highlight_needs_tick(),
                "worker stopped pending without applying a result"
            );
            assert!(
                Instant::now() < deadline,
                "worker did not return before the bounded deadline"
            );
            std::thread::yield_now();
        }

        let refined = state
            .refined_diff_styles(0, "scoped-edit")
            .expect("exact message refinement");
        assert!(refined.contains_key(&3));
        assert!(refined.contains_key(&4));
        let warm = crate::ui::build_lines_for_message(
            &state.messages[0],
            &theme,
            80,
            0,
            false,
            Some(refined),
        );
        let cold_field = inserted_source_line(&cold, "    field = 1");
        let warm_field = inserted_source_line(&warm, "    field = 1");

        assert_ne!(warm_field.spans[1..], cold_field.spans[1..]);
        assert_eq!(
            normalized_source_spans(&warm_field.spans[1..]),
            normalized_source_spans(&refined[&4])
        );
        assert!(!state.edit_highlight_needs_tick());
    }

    #[test]
    fn failed_result_finishes_pending_without_touching_or_noise() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        let revisions_before = state.message_revisions.clone();
        let messages_before = state.messages.len();

        assert!(!state.apply_edit_highlight_result(
            crate::edit_highlight_worker::EditHighlightResult {
                job: job.clone(),
                outcome: crate::edit_highlight_worker::EditHighlightOutcome::Failed,
            }
        ));

        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
        assert_eq!(state.message_revisions, revisions_before);
        assert_eq!(state.messages.len(), messages_before);
    }

    #[test]
    fn stale_edit_highlight_identity_is_rejected_without_touching_message() {
        type Mutation =
            Box<dyn Fn(&mut AppState, &mut crate::edit_highlight_worker::EditHighlightJob)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|state, job| {
                state.touch_message(job.message_index);
            }),
            Box::new(|_, job| job.job_id += 1),
            Box::new(|_, job| job.message_index += 1),
            Box::new(|_, job| job.tool_id = "other-tool".to_string()),
            Box::new(|_, job| job.absolute_path = PathBuf::from("/other/item.py")),
            Box::new(|_, job| job.display_path = "src/other.py".to_string()),
            Box::new(|state, _| {
                state.set_syntax_theme_for_test(crate::syntax_highlight::SyntaxTheme::OneHalfLight);
            }),
            Box::new(|_, job| {
                job.syntax_theme_revision = crate::terminal_capabilities::syntax_style_revision(
                    crate::syntax_highlight::SyntaxTheme::OneHalfLight,
                    job.syntax_color_level,
                );
            }),
            Box::new(|state, job| {
                let ChatMessage::ToolCall { diff, .. } = &mut state.messages[job.message_index]
                else {
                    unreachable!();
                };
                *diff = Some(EDIT_DIFF.replace("value = 2", "value = 3"));
            }),
            Box::new(|state, job| {
                let ChatMessage::ToolCall { target, .. } = &mut state.messages[job.message_index]
                else {
                    unreachable!();
                };
                *target = Some("src/other.py".to_string());
            }),
        ];

        for mutate in mutations {
            let (_directory, mut state, mut job) = state_with_submitted_edit_job();
            mutate(&mut state, &mut job);
            let revisions_before = state.message_revisions.clone();

            assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
            assert!(
                state
                    .refined_diff_styles(job.message_index, &job.tool_id)
                    .is_none()
            );
            assert_eq!(state.message_revisions, revisions_before);
        }
    }

    #[test]
    fn stale_edit_highlight_is_rejected_when_only_syntax_color_level_changes() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        let syntax_theme = state.syntax_theme_for_test();
        state.set_syntax_color_level_for_test(
            crate::terminal_capabilities::TerminalColorLevel::Ansi256,
        );
        let revisions_before = state.message_revisions.clone();

        assert_eq!(state.syntax_theme_for_test(), syntax_theme);
        assert_ne!(state.syntax_color_level_for_test(), job.syntax_color_level);
        assert_ne!(
            crate::terminal_capabilities::syntax_style_revision(
                state.syntax_theme_for_test(),
                state.syntax_color_level_for_test(),
            ),
            job.syntax_theme_revision
        );
        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
        assert_eq!(state.message_revisions, revisions_before);
    }

    #[test]
    fn ready_result_rejects_current_failed_status_without_touching() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        let ChatMessage::ToolCall { status, .. } = &mut state.messages[job.message_index] else {
            unreachable!();
        };
        *status = "failed".to_string();
        let revisions = state.message_revisions.clone();

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert_eq!(state.message_revisions, revisions);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
    }

    #[test]
    fn ready_result_rejects_current_row_tool_id_and_finishes_pending() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        let ChatMessage::ToolCall { id, .. } = &mut state.messages[job.message_index] else {
            unreachable!();
        };
        *id = "different-current-id".to_string();
        let revisions = state.message_revisions.clone();

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert_eq!(state.message_revisions, revisions);
        assert!(state.edit_highlights.applied().is_empty());
    }

    #[test]
    fn ready_result_rejects_current_diff_destination_mismatch() {
        let (directory, mut state, job) = state_with_submitted_edit_job();
        std::fs::write(directory.path().join("src/other.py"), "value = 2\n")
            .expect("other post-edit file");
        let ChatMessage::ToolCall { diff, .. } = &mut state.messages[job.message_index] else {
            unreachable!();
        };
        *diff = Some(EDIT_DIFF.replace("src/item.py", "src/other.py"));
        let revisions = state.message_revisions.clone();

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert_eq!(state.message_revisions, revisions);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn ready_result_rejects_retargeted_symlink_on_apply_path() {
        use std::os::unix::fs::symlink;

        let (directory, mut state, job) = real_alias_edit_state();
        std::fs::write(directory.path().join("src/other.py"), "value = 2\n")
            .expect("other post-edit file");
        let alias = directory.path().join("src/alias.py");
        assert_eq!(job.display_path, "src/alias.py");
        std::fs::remove_file(&alias).expect("remove initial alias");
        symlink(directory.path().join("src/other.py"), &alias).expect("retarget alias");
        let revisions = state.message_revisions.clone();

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert_eq!(state.message_revisions, revisions);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
    }

    #[test]
    fn stale_result_does_not_remove_newer_pending_job_for_same_tool() {
        let (_directory, mut state, stale_job) = state_with_submitted_edit_job();
        submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
        let newer_job = state
            .pending_edit_highlight_job("edit-1")
            .expect("newer pending job");
        assert_ne!(stale_job.job_id, newer_job.job_id);

        assert!(!state.apply_edit_highlight_result(ready_result(stale_job)));
        assert_eq!(
            state
                .pending_edit_highlight_job("edit-1")
                .expect("newer job preserved")
                .job_id,
            newer_job.job_id
        );
    }

    #[test]
    fn touch_mutate_and_replace_cancel_only_their_exact_pending_message() {
        for action in ["touch", "mutate", "replace"] {
            let (_directory, mut state) = configured_edit_state();
            submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
            let job_a = state
                .pending_edit_highlight_job("edit-a")
                .expect("pending A");
            submit_live_edit(&mut state, "edit-b", "src/item.py", EDIT_DIFF);
            let job_b = state
                .pending_edit_highlight_job("edit-b")
                .expect("pending B");
            assert_eq!(state.pending_edit_highlight_count(), 2);

            match action {
                "touch" => {
                    assert!(state.touch_message(job_a.message_index));
                }
                "mutate" => {
                    state
                        .mutate_message(job_a.message_index, |message| {
                            let ChatMessage::ToolCall { expanded, .. } = message else {
                                unreachable!();
                            };
                            *expanded = true;
                        })
                        .expect("mutate A");
                }
                "replace" => {
                    let replacement = state.messages[job_a.message_index].clone();
                    assert!(state.replace_message(job_a.message_index, replacement));
                }
                _ => unreachable!(),
            }

            assert!(
                state.pending_edit_highlight_job("edit-a").is_none(),
                "{action}"
            );
            assert_eq!(
                state
                    .pending_edit_highlight_job("edit-b")
                    .expect("unrelated B remains")
                    .job_id,
                job_b.job_id,
                "{action}"
            );
            assert_eq!(state.pending_edit_highlight_count(), 1, "{action}");
            assert!(state.edit_highlight_needs_tick(), "{action}");
            assert!(state.apply_edit_highlight_result(ready_result(job_b.clone())));
            assert!(
                state
                    .refined_diff_styles(job_b.message_index, &job_b.tool_id)
                    .is_some()
            );
        }
    }

    #[test]
    fn replacing_non_tool_message_keeps_unrelated_edit_pending() {
        let (_directory, mut state) = configured_edit_state();
        state.push_message(ChatMessage::Reasoning("old".to_string()));
        submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
        let job = state
            .pending_edit_highlight_job("edit-a")
            .expect("pending edit");

        assert!(state.replace_message(0, ChatMessage::Reasoning("new".to_string())));

        assert_eq!(
            state
                .pending_edit_highlight_job("edit-a")
                .expect("edit pending survives")
                .job_id,
            job.job_id
        );
        assert!(state.edit_highlight_needs_tick());
        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_some()
        );
    }

    #[test]
    fn disconnected_worker_is_abandoned_silently_and_next_edit_respawns() {
        fn disconnected(
            _runtime: &mut crate::edit_highlight_worker::EditHighlightRuntime,
        ) -> crate::edit_highlight_worker::DrainResults {
            crate::edit_highlight_worker::DrainResults {
                results: Vec::new(),
                disconnected: true,
            }
        }

        let (_directory, mut state, _job) = state_with_submitted_edit_job();
        let revisions_before = state.message_revisions.clone();
        let messages_before = state.messages.len();
        state.set_edit_highlight_drain_for_test(Some(disconnected));

        assert!(!state.poll_edit_highlight_results());
        assert!(!state.edit_highlight_runtime_started());
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert_eq!(state.message_revisions, revisions_before);
        assert_eq!(state.messages.len(), messages_before);

        state.set_edit_highlight_drain_for_test(None);
        submit_live_edit(&mut state, "edit-2", "src/item.py", EDIT_DIFF);
        assert!(state.edit_highlight_runtime_started());
        assert_eq!(state.pending_edit_highlight_count(), 1);
    }

    #[test]
    fn tool_touch_mutate_and_replace_remove_applied_map_before_revision_change() {
        for action in ["touch", "mutate", "replace"] {
            let (_directory, mut state, job) = state_with_submitted_edit_job();
            assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
            let revision = state.message_revisions[job.message_index];

            match action {
                "touch" => {
                    state.touch_message(job.message_index);
                }
                "mutate" => {
                    state.mutate_message(job.message_index, |message| {
                        let ChatMessage::ToolCall { expanded, .. } = message else {
                            unreachable!();
                        };
                        *expanded = true;
                    });
                }
                "replace" => {
                    let replacement = state.messages[job.message_index].clone();
                    state.replace_message(job.message_index, replacement);
                }
                _ => unreachable!(),
            }

            assert!(state.message_revisions[job.message_index] > revision);
            assert!(
                state
                    .refined_diff_styles(job.message_index, &job.tool_id)
                    .is_none()
            );
        }
    }

    #[test]
    fn message_lifecycle_prunes_applied_maps_and_pending_jobs() {
        for action in ["clear", "replace", "truncate", "retain"] {
            let (_directory, mut state, job) = state_with_submitted_edit_job();
            assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
            submit_live_edit(&mut state, "edit-2", "src/item.py", EDIT_DIFF);
            assert_eq!(state.pending_edit_highlight_count(), 1);

            match action {
                "clear" => state.clear_messages(),
                "replace" => state.replace_messages([ChatMessage::System("new".to_string())]),
                "truncate" => state.truncate_messages(job.message_index),
                "retain" => state
                    .retain_messages(|message| !matches!(message, ChatMessage::ToolCall { .. })),
                _ => unreachable!(),
            }

            assert!(
                state
                    .refined_diff_styles(job.message_index, &job.tool_id)
                    .is_none()
            );
            assert_eq!(state.pending_edit_highlight_count(), 0);
        }
    }

    #[test]
    fn retained_reindexing_clears_all_pending_jobs_conservatively() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        state
            .messages
            .insert(0, ChatMessage::System("remove".to_string()));
        state.reconcile_message_tracking();
        assert_eq!(state.pending_edit_highlight_count(), 1);

        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
        );

        assert_eq!(state.pending_edit_highlight_count(), 0);
        let revisions = state.message_revisions.clone();
        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert_eq!(state.message_revisions, revisions);
        assert!(
            state
                .refined_diff_styles(job.message_index, &job.tool_id)
                .is_none()
        );
    }

    #[test]
    fn removed_message_result_and_reused_identity_never_inherit_styles() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        state.clear_messages();
        state.push_message(ChatMessage::ToolCall {
            id: job.tool_id.clone(),
            name: "edit".to_string(),
            target: Some(job.display_path.clone()),
            status: "completed".to_string(),
            output: None,
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
            expanded: false,
        });

        assert!(!state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(state.refined_diff_styles(0, &job.tool_id).is_none());
    }

    #[test]
    fn direct_push_with_reused_tool_id_does_not_inherit_applied_styles() {
        let (_directory, mut state, job) = state_with_submitted_edit_job();
        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));

        state.push_message(ChatMessage::ToolCall {
            id: job.tool_id.clone(),
            name: "edit".to_string(),
            target: Some(job.display_path.clone()),
            status: "running".to_string(),
            output: None,
            diff: None,
            kind: None,
            expanded: false,
        });

        assert!(state.refined_diff_styles(0, &job.tool_id).is_none());
        assert!(state.refined_diff_styles(1, &job.tool_id).is_none());
    }

    #[test]
    fn duplicate_tool_id_map_is_bound_to_exact_message_revision() {
        let (_directory, mut state) = configured_edit_state();
        state.push_message(ChatMessage::ToolCall {
            id: "duplicate".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
            status: "completed".to_string(),
            output: Some("older".to_string()),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
            expanded: false,
        });
        submit_live_edit(&mut state, "duplicate", "src/item.py", EDIT_DIFF);
        let job = state
            .pending_edit_highlight_job("duplicate")
            .expect("newer duplicate job");
        assert_eq!(job.message_index, 1);

        assert!(state.apply_edit_highlight_result(ready_result(job.clone())));
        assert!(
            AppState::refined_diff_styles_for_message(
                &state.message_revisions,
                state.edit_highlights.applied(),
                0,
                &state.messages[0],
            )
            .is_none()
        );
        assert!(
            AppState::refined_diff_styles_for_message(
                &state.message_revisions,
                state.edit_highlights.applied(),
                1,
                &state.messages[1],
            )
            .is_some()
        );

        state.truncate_messages(1);

        assert!(state.refined_diff_styles(0, "duplicate").is_none());
        assert!(state.edit_highlights.applied().is_empty());
    }

    #[test]
    fn partial_prune_keeps_unrelated_applied_revision() {
        let (_directory, mut state) = configured_edit_state();
        submit_live_edit(&mut state, "edit-a", "src/item.py", EDIT_DIFF);
        let job_a = state
            .pending_edit_highlight_job("edit-a")
            .expect("pending A");
        submit_live_edit(&mut state, "edit-b", "src/item.py", EDIT_DIFF);
        let job_b = state
            .pending_edit_highlight_job("edit-b")
            .expect("pending B");
        assert!(state.apply_edit_highlight_result(ready_result(job_a)));
        assert!(state.apply_edit_highlight_result(ready_result(job_b)));
        assert!(state.refined_diff_styles(0, "edit-a").is_some());
        assert!(state.refined_diff_styles(1, "edit-b").is_some());

        state.truncate_messages(1);

        assert!(state.refined_diff_styles(0, "edit-a").is_some());
        assert_eq!(state.edit_highlights.applied().len(), 1);
    }

    #[test]
    fn reused_tool_id_live_submission_applies_only_to_new_row() {
        let (_directory, mut state, first_job) = state_with_submitted_edit_job();
        assert!(state.apply_edit_highlight_result(ready_result(first_job)));
        assert!(state.refined_diff_styles(0, "edit-1").is_some());

        submit_live_edit(&mut state, "edit-1", "src/item.py", EDIT_DIFF);
        let new_job = state
            .pending_edit_highlight_job("edit-1")
            .expect("new reused job");
        assert_eq!(new_job.message_index, 1);
        assert!(state.apply_edit_highlight_result(ready_result(new_job)));

        assert!(state.refined_diff_styles(0, "edit-1").is_none());
        assert!(state.refined_diff_styles(1, "edit-1").is_some());
    }

    #[test]
    fn disconnected_job_sender_drops_runtime_without_noise_or_extra_revision() {
        fn disconnected_runtime()
        -> std::io::Result<crate::edit_highlight_worker::EditHighlightRuntime> {
            Ok(crate::edit_highlight_worker::EditHighlightRuntime::disconnected_for_test())
        }

        let (_directory, mut state) = configured_edit_state();
        state.set_edit_highlight_runtime_factory_for_test(disconnected_runtime);
        state.update(TuiEvent::ToolRequested {
            id: "send-failure".to_string(),
            name: "edit".to_string(),
            target: Some("src/item.py".to_string()),
        });
        let revision_before_completion = state.message_revisions[0];

        state.update(TuiEvent::ToolCompleted {
            id: "send-failure".to_string(),
            name: "edit".to_string(),
            status: "completed".to_string(),
            output: "edited src/item.py".to_string(),
            diff: Some(EDIT_DIFF.to_string()),
            kind: None,
        });

        assert_eq!(state.messages.len(), 1);
        assert_eq!(
            state.message_revisions[0],
            revision_before_completion.saturating_add(1)
        );
        assert!(matches!(
            &state.messages[0],
            ChatMessage::ToolCall {
                status,
                output: Some(output),
                ..
            } if status == "completed" && output == "edited src/item.py"
        ));
        assert_eq!(state.pending_edit_highlight_count(), 0);
        assert_eq!(state.successful_edit_highlight_submit_count(), 0);
        assert!(!state.edit_highlight_runtime_started());
    }
}
