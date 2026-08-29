use crossbeam_channel as mpsc;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orca_core::approval_types::ApprovalMode;
#[cfg(test)]
use orca_core::cost_types::UsageTotals;
#[cfg(test)]
use orca_core::goal_types::ThreadGoal;
#[cfg(test)]
use orca_core::plan_types::PlanItem;
use orca_core::proposed_plan::ProposedPlanStreamParser;
#[cfg(test)]
use orca_core::task_types::BackgroundTaskSummary;
use orca_file_search::{SearchPhase, SearchProgress};
use orca_runtime::history::SessionSummary;
use orca_runtime::mentions::{MentionBindings, MentionCandidate};
use orca_runtime::runtime_permission::RuntimePermissionRequestKind;
#[cfg(test)]
use orca_runtime::surface::SurfaceOperationId;

use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::edit_highlight::EditHighlightState;
#[cfg(test)]
use crate::edit_highlight::parsed_diff_structure_matches_target;
use crate::image_preview::{ImageHitArea, ImageRenderState, ImageViewerState};
use crate::input_history::load_input_history;
use crate::interaction_state::InteractionState;
use crate::plan_panel::PlanPanelState;
use crate::queued_input::QueuedSubmissionState;
#[cfg(test)]
#[doc(hidden)]
pub use crate::surface_projection::SurfaceProjectionState;
use crate::surface_projection::{
    SurfaceGoalProjectionState, SurfaceMetricsState, SurfaceOperationProjectionState,
    SurfaceSessionProjectionState, SurfaceWorkflowTaskProjectionState,
};
pub use crate::transcript_state::ChatMessage;
use crate::transcript_state::TranscriptState;
use crate::transcript_view::TranscriptRenderCache;
#[cfg(test)]
use crate::transcript_view::TranscriptRenderContext;
use crate::user_input_dialog::UserInputDialog;
pub use crate::viewport_state::CopyNotice;
use crate::viewport_state::ViewportState;
#[cfg(test)]
use crate::workflow_panel::sort_workflow_tasks_for_panel;
use crate::workflow_panel::{WorkflowPanelState, push_pending_workflow_notification_unique};
use crate::workspace_status::GitIdentity;

pub(crate) use crate::interaction_state::PendingInteractionSubmission;
pub(crate) use crate::protocol::SessionAttachmentId;
pub use crate::protocol::{
    AttachedTuiEvent, GoalDraft, PendingTuiInput, PendingWorkflowNotification, TuiEvent,
    TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse, TuiMcpElicitationMode,
    TuiMemoryScope, TuiTaskLifecycle, UserAction,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalDialog {
    pub plan: String,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDialog {
    pub selected: usize,
    pub model: String,
    pub reasoning_effort: orca_core::config::ReasoningEffort,
    pub approval_mode: ApprovalMode,
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
    pub(crate) transcript: TranscriptState,
    pub(crate) image_viewer: Option<ImageViewerState>,
    pub(crate) image_renderer: ImageRenderState,
    pub(crate) image_hit_areas: Vec<ImageHitArea>,
    pub status: AppStatus,
    pub running_started_at: Option<Instant>,
    pub(crate) viewport: ViewportState,
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
    pub config_dialog: Option<ConfigDialog>,
    pub(crate) user_input_dialog: Option<UserInputDialog>,
    pub(crate) interaction: InteractionState,
    /// Tool / "tool\u{0}target" keys the user chose to always allow this
    /// session. Checked when a new approval arrives so the dialog is skipped.
    pub approval_allowlist: std::collections::HashSet<String>,
    pub setup_step: u8,
    pub show_shortcuts: bool,
    pub input_history: Vec<String>,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub(crate) composer_images: ComposerImageState,
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
    pub(crate) surface_goal: SurfaceGoalProjectionState,
    pub(crate) surface_operation: SurfaceOperationProjectionState,
    pub(crate) surface_workflow_tasks: SurfaceWorkflowTaskProjectionState,
    pub recovery_prompt_visible: bool,
    pub recovery_prompt_selected: usize,
    pub panel_mode: PanelMode,
    pub(crate) workflow_panel: WorkflowPanelState,
    pub pending_workflow_notifications: VecDeque<PendingWorkflowNotification>,
    pub suppress_background_main_session_output: bool,
    pub tick: u64,
    pub(crate) edit_highlights: EditHighlightState,
}

pub trait ScrollAmount {
    fn as_usize(self) -> usize;
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
    #[cfg(test)]
    pub(crate) fn stage_pending_interaction_submission(
        &mut self,
        visible_text: String,
    ) -> Option<TuiInteractionKey> {
        self.stage_pending_interaction_submission_with_composer(
            visible_text,
            self.mention_bindings.clone(),
            self.atomic_skill_tokens.clone(),
            self.pending_pastes.clone(),
        )
    }

    pub(crate) fn stage_pending_interaction_submission_with_composer(
        &mut self,
        visible_text: String,
        mention_bindings: MentionBindings,
        atomic_skill_tokens: MentionBindings,
        pending_pastes: Vec<(String, String)>,
    ) -> Option<TuiInteractionKey> {
        let pending_input = self.interaction.pending_input.clone()?;
        let key = pending_input.key().clone();
        self.interaction.pending_submission = Some(PendingInteractionSubmission {
            key: key.clone(),
            pending_input,
            mcp_mode: self.interaction.pending_mcp_elicitation_mode.clone(),
            visible_text,
            mention_bindings,
            atomic_skill_tokens,
            pending_pastes,
            user_input_dialog: self.user_input_dialog.clone(),
        });
        Some(key)
    }

    pub(crate) fn discard_pending_interaction_submission(
        &mut self,
        key: &TuiInteractionKey,
    ) -> bool {
        if self
            .interaction
            .pending_submission
            .as_ref()
            .is_some_and(|submission| &submission.key == key)
        {
            self.interaction.pending_submission = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn restore_pending_interaction_submission(
        &mut self,
        key: &TuiInteractionKey,
        message: String,
    ) -> Option<String> {
        let submission = self.interaction.pending_submission.take()?;
        if submission.key != *key {
            self.interaction.pending_submission = Some(submission);
            return None;
        }
        self.interaction.pending_input = Some(submission.pending_input);
        self.interaction.pending_mcp_elicitation_mode = submission.mcp_mode;
        self.mention_bindings = submission.mention_bindings;
        self.atomic_skill_tokens = submission.atomic_skill_tokens;
        self.pending_pastes = submission.pending_pastes;
        self.user_input_dialog = submission.user_input_dialog;
        self.reset_assistant_stream();
        self.clear_receiving_tool_progress();
        self.push_message(ChatMessage::Error(message));
        self.set_status(AppStatus::WaitingUserInput);
        Some(submission.visible_text)
    }

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
            transcript: TranscriptState::new(),
            image_viewer: None,
            image_renderer: ImageRenderState::default(),
            image_hit_areas: Vec::new(),
            status: AppStatus::Idle,
            running_started_at: None,
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
            config_dialog: None,
            user_input_dialog: None,
            interaction: InteractionState::default(),
            approval_allowlist: std::collections::HashSet::new(),
            setup_step: 0,
            show_shortcuts: false,
            input_history: load_input_history(),
            pending_pastes: Vec::new(),
            composer_images: ComposerImageState::default(),
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
            surface_goal: SurfaceGoalProjectionState::default(),
            surface_operation: SurfaceOperationProjectionState::default(),
            surface_workflow_tasks: SurfaceWorkflowTaskProjectionState::default(),
            recovery_prompt_visible: false,
            recovery_prompt_selected: 0,
            panel_mode: PanelMode::Conversation,
            workflow_panel: WorkflowPanelState::default(),
            pending_workflow_notifications: VecDeque::new(),
            suppress_background_main_session_output: false,
            tick: 0,
            viewport: ViewportState::default(),
            edit_highlights: EditHighlightState::default(),
        }
    }

    /// Map a mouse position to transcript content space; `None` outside the
    /// transcript area (or before the first frame rendered it).
    pub(crate) fn transcript_pos_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::selection::SelectionPos> {
        let area = self.viewport.transcript_area?;
        crate::selection::screen_to_selection_pos(
            area,
            self.viewport.viewport_base_row,
            column,
            row,
        )
    }

    /// Like [`Self::transcript_pos_at`] but clamps to the nearest transcript
    /// cell, so drags that leave the area keep tracking.
    pub(crate) fn transcript_pos_at_clamped(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::selection::SelectionPos> {
        let area = self.viewport.transcript_area?;
        crate::selection::screen_to_selection_pos_clamped(
            area,
            self.viewport.viewport_base_row,
            column,
            row,
        )
    }

    /// The transcript cache mouse interaction should read from: the welcome
    /// cache while no messages exist, the live transcript cache afterwards.
    pub(crate) fn active_transcript_cache(&self) -> &TranscriptRenderCache {
        if self.transcript.messages.is_empty() {
            &self.transcript.welcome_render_cache
        } else {
            &self.transcript.render_cache
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
        self.viewport.copy_notice = Some(CopyNotice {
            chars: text.chars().count(),
            at: now,
            // Terminals cap OSC 52 length; the app loop will skip the remote
            // write for oversized text, so the notice must not overclaim.
            local_only: text.len() > crate::clipboard::OSC52_MAX_TEXT_BYTES,
        });
        self.viewport.pending_clipboard_copy = Some(text);
    }

    /// The staged copy notice while it is still fresh.
    pub fn copy_notice_at(&self, now: Instant) -> Option<CopyNotice> {
        self.viewport
            .copy_notice
            .filter(|notice| now.duration_since(notice.at) < Self::COPY_NOTICE_TTL)
    }

    /// One animation-tick step of edge-drag auto-scroll: scroll a line and
    /// grow the selection head one content row in the drag direction. The
    /// head moves in content space, so it stays glued to what it selected.
    pub(crate) fn apply_drag_edge_scroll(&mut self) {
        let Some((direction, column)) = self.viewport.drag_edge_scroll else {
            return;
        };
        let col = self
            .viewport
            .transcript_area
            .map(|area| column.saturating_sub(area.x) as usize)
            .unwrap_or(0);
        let Some(mut selection) = self
            .viewport
            .selection
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        if direction < 0 {
            self.scroll_up(1usize);
            selection.head.row = selection.head.row.saturating_sub(1);
        } else {
            self.scroll_down(1usize);
            let last_row = self.viewport.total_lines.saturating_sub(1);
            selection.head.row = selection.head.row.saturating_add(1).min(last_row);
        }
        selection.head.col = col;
        self.viewport.selection = Some(selection);
    }

    /// Drop the mouse selection (and any armed edge auto-scroll).
    ///
    /// Called whenever the transcript's visual row space changes shape —
    /// terminal resize (re-wrap), message removal/replacement, or an in-place
    /// rewrite of a non-tail message. Positions kept from the old row space
    /// would highlight and copy unrelated content.
    pub(crate) fn invalidate_selection(&mut self) {
        self.viewport.selection = None;
        self.viewport.drag_edge_scroll = None;
    }

    fn allocate_message_revision(&mut self) -> u64 {
        let revision = self.transcript.next_message_revision;
        self.transcript.next_message_revision =
            self.transcript.next_message_revision.wrapping_add(1).max(1);
        revision
    }

    pub(crate) fn reconcile_message_tracking(&mut self) {
        let structure_changed =
            self.transcript.message_revisions.len() != self.transcript.messages.len();
        if self.transcript.message_revisions.len() > self.transcript.messages.len() {
            self.transcript
                .message_revisions
                .truncate(self.transcript.messages.len());
            self.transcript
                .render_cache
                .truncate(self.transcript.messages.len());
            self.invalidate_selection();
        }
        while self.transcript.message_revisions.len() < self.transcript.messages.len() {
            let revision = self.allocate_message_revision();
            self.transcript.message_revisions.push(revision);
        }
        self.transcript
            .render_cache
            .reconcile_len(self.transcript.messages.len());
        if structure_changed {
            self.rebuild_tool_call_indices();
            self.assert_tool_call_index_consistent();
        }
    }

    fn reset_message_tracking(&mut self) {
        self.transcript.message_revisions.clear();
        self.transcript.render_cache.clear();
        while self.transcript.message_revisions.len() < self.transcript.messages.len() {
            let revision = self.allocate_message_revision();
            self.transcript.message_revisions.push(revision);
        }
        self.transcript
            .render_cache
            .reconcile_len(self.transcript.messages.len());
        self.rebuild_tool_call_indices();
        self.assert_tool_call_index_consistent();
    }

    fn rebuild_tool_call_indices(&mut self) {
        self.transcript.tool_call_indices.clear();
        for (index, message) in self.transcript.messages.iter().enumerate() {
            if let ChatMessage::ToolCall { id, .. } = message {
                self.transcript
                    .tool_call_indices
                    .entry(id.clone())
                    .or_insert(index);
            }
        }
    }

    pub(crate) fn tool_call_message_index(&self, id: &str) -> Option<usize> {
        self.transcript.tool_call_indices.get(id).copied()
    }

    #[cfg(any(test, debug_assertions))]
    fn assert_tool_call_index_consistent(&self) {
        let mut canonical = HashMap::new();
        for (index, message) in self.transcript.messages.iter().enumerate() {
            if let ChatMessage::ToolCall { id, .. } = message {
                canonical.entry(id.clone()).or_insert(index);
            }
        }
        debug_assert_eq!(self.transcript.tool_call_indices, canonical);
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn assert_tool_call_index_consistent(&self) {}

    #[cfg(test)]
    fn assert_surface_projection_consistent(&self, projection: &SurfaceProjectionState) {
        self.surface_session.assert_matches_projection(projection);
        self.surface_metrics.assert_matches_projection(projection);
        self.surface_workflow_tasks
            .assert_matches_projection(projection);
        debug_assert_eq!(
            self.workflow_tasks(),
            sort_workflow_tasks_for_panel(projection.workflow_tasks.clone())
        );
        self.surface_goal.assert_matches_projection(projection);
        self.surface_operation.assert_matches_projection(projection);
    }

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
            self.transcript
                .tool_call_indices
                .entry(id.clone())
                .or_insert(self.transcript.messages.len());
        }
        self.transcript.messages.push(message);
        self.transcript.message_revisions.push(revision);
        self.transcript
            .render_cache
            .reconcile_len(self.transcript.messages.len());
        // A message landed below the viewport while the user was scrolled
        // up: feed the jump pill's unread count.
        if !self.viewport.auto_scroll {
            self.viewport.unseen_messages = self.viewport.unseen_messages.saturating_add(1);
        }
    }

    pub(crate) fn push_user_message_with_images(
        &mut self,
        text: String,
        images: &[ComposerImageAttachment],
    ) {
        self.push_message(ChatMessage::User(text));
        for image in images {
            self.push_message(ChatMessage::Image(image.preview()));
        }
    }

    pub(crate) fn begin_image_render_frame(&mut self) {
        self.image_hit_areas = Vec::new();
    }

    pub(crate) fn replace_messages(&mut self, messages: impl IntoIterator<Item = ChatMessage>) {
        self.reset_assistant_stream();
        self.reset_queued_user_messages();
        self.transcript.messages = messages.into_iter().collect();
        self.clear_applied_edit_highlights();
        self.clear_pending_edit_highlights();
        self.reset_message_tracking();
        self.transcript.finalized_count = 0;
        self.transcript.flushed_count = 0;
        self.viewport.unseen_messages = 0;
        self.invalidate_selection();
    }

    pub(crate) fn clear_messages(&mut self) {
        self.reset_assistant_stream();
        self.reset_queued_user_messages();
        self.transcript.search.reset();
        self.transcript.messages.clear();
        self.transcript.message_revisions.clear();
        self.transcript.tool_call_indices.clear();
        self.transcript.render_cache.clear();
        self.clear_applied_edit_highlights();
        self.clear_pending_edit_highlights();
        self.transcript.finalized_count = 0;
        self.transcript.flushed_count = 0;
        self.viewport.unseen_messages = 0;
        self.invalidate_selection();
    }

    pub(crate) fn reset_session_projection(&mut self) {
        self.surface_session.reset();
        self.clear_messages();
        self.clear_plan_panel();
        self.transcript.proposed_plan_parser = ProposedPlanStreamParser::default();
        self.surface_goal.reset();
        self.surface_operation.reset();
        self.surface_workflow_tasks.reset();
        self.recovery_prompt_visible = false;
        self.recovery_prompt_selected = 0;
        self.surface_metrics.reset();
        self.approval_dialog = None;
        self.interaction.pending_input = None;
        self.interaction.pending_mcp_elicitation_mode = None;
        self.interaction.pending_submission = None;
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
        self.config_dialog = None;
        self.user_input_dialog = None;
        self.pre_plan_approval_mode = None;
        self.pending_pastes.clear();
        self.composer_images.reset_for_new_session();
        self.reset_history_navigation();
        self.last_ctrl_c = None;
        self.panel_mode = PanelMode::Conversation;
        self.reset_workflow_panel();
        self.pending_workflow_notifications.clear();
        self.suppress_background_main_session_output = false;
        self.last_completed_at = None;
        self.viewport.pending_clipboard_copy = None;
        self.viewport.last_left_click = None;
        self.viewport.copy_notice = None;
        self.viewport.composer_mouse_selecting = false;
        self.viewport.scroll_offset = 0;
        self.viewport.auto_scroll = true;
        self.set_status(AppStatus::Idle);
    }

    pub(crate) fn truncate_messages(&mut self, len: usize) {
        if self
            .transcript
            .assistant_stream_tail
            .is_none_or(|tail_index| tail_index >= len)
        {
            self.reset_assistant_stream();
        }
        self.reconcile_message_tracking();
        let did_truncate = len < self.transcript.messages.len();
        if did_truncate {
            self.invalidate_selection();
            self.clear_pending_edit_highlights();
        }
        self.transcript.messages.truncate(len);
        self.transcript.message_revisions.truncate(len);
        self.transcript.render_cache.truncate(len);
        self.transcript.finalized_count = self.transcript.finalized_count.min(len);
        self.transcript.flushed_count = self.transcript.flushed_count.min(len);
        self.prune_applied_diff_highlights();
        if did_truncate {
            self.rebuild_tool_call_indices();
        }
    }

    pub(crate) fn replace_message(&mut self, index: usize, message: ChatMessage) -> bool {
        self.reconcile_message_tracking();
        if index >= self.transcript.messages.len() {
            return false;
        }
        self.remove_applied_highlight_for_message(index);
        let previous_tool_id =
            self.transcript
                .messages
                .get(index)
                .and_then(|message| match message {
                    ChatMessage::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                });
        let next_tool_id = match &message {
            ChatMessage::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.transcript.messages[index] = message;
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
        let previous_tool_id =
            self.transcript
                .messages
                .get(index)
                .and_then(|message| match message {
                    ChatMessage::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                });
        let result = mutate(self.transcript.messages.get_mut(index)?);
        let current_tool_id =
            self.transcript
                .messages
                .get(index)
                .and_then(|message| match message {
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
        if index >= self.transcript.message_revisions.len() {
            return false;
        }
        let old_revision = self.transcript.message_revisions[index];
        self.cancel_pending_edit_highlight_for_message(index, old_revision);
        self.remove_applied_highlight_for_message(index);
        // Rewriting any message but the tail can change its height and shift
        // every row below it. Tail rewrites (streaming deltas) leave earlier
        // rows in place, so a selection above the stream stays valid.
        if index + 1 != self.transcript.messages.len() {
            self.invalidate_selection();
        }
        let revision = self.allocate_message_revision();
        self.transcript.message_revisions[index] = revision;
        self.transcript.render_cache.invalidate(index);
        true
    }

    pub(crate) fn retain_messages(&mut self, mut keep: impl FnMut(&ChatMessage) -> bool) {
        self.reconcile_message_tracking();
        let messages = std::mem::take(&mut self.transcript.messages);
        let revisions = std::mem::take(&mut self.transcript.message_revisions);
        let active_tail = self.transcript.assistant_stream_tail;
        let mut retained_tail = None;
        let finalized_count = self.transcript.finalized_count.min(messages.len());
        let flushed_count = self.transcript.flushed_count.min(messages.len());
        let mut retained_finalized = 0;
        let mut retained_flushed = 0;
        let mut retained_mask = Vec::with_capacity(messages.len());
        let mut removed_tool_revisions = Vec::new();
        for (index, (message, revision)) in messages.into_iter().zip(revisions).enumerate() {
            let retain = keep(&message);
            retained_mask.push(retain);
            if retain {
                if active_tail == Some(index) {
                    retained_tail = Some(self.transcript.messages.len());
                }
                retained_finalized += usize::from(index < finalized_count);
                retained_flushed += usize::from(index < flushed_count);
                self.transcript.messages.push(message);
                self.transcript.message_revisions.push(revision);
            } else if matches!(message, ChatMessage::ToolCall { .. }) {
                removed_tool_revisions.push(revision);
            }
        }
        self.transcript.render_cache.retain(&retained_mask);
        self.rebuild_tool_call_indices();
        if active_tail.is_some() {
            if retained_tail.is_some() {
                self.transcript.assistant_stream_tail = retained_tail;
            } else {
                self.reset_assistant_stream();
            }
        }
        self.transcript.finalized_count = retained_finalized;
        self.transcript.flushed_count = retained_flushed;
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
        self.config_dialog = None;
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
        if self.viewport.total_lines <= self.viewport.visible_height {
            return;
        }
        self.viewport.scroll_offset = self.viewport.scroll_offset.saturating_sub(lines);
        self.viewport.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, lines: impl ScrollAmount) {
        let lines = lines.as_usize();
        let max_scroll = self
            .viewport
            .total_lines
            .saturating_sub(self.viewport.visible_height);
        self.viewport.scroll_offset = self
            .viewport
            .scroll_offset
            .saturating_add(lines)
            .min(max_scroll);
        if self.viewport.scroll_offset >= max_scroll {
            self.viewport.auto_scroll = true;
            // Back at the tail: nothing below is unseen any more.
            self.viewport.unseen_messages = 0;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        let max_scroll = self
            .viewport
            .total_lines
            .saturating_sub(self.viewport.visible_height);
        self.viewport.scroll_offset = max_scroll;
        self.viewport.auto_scroll = true;
        self.viewport.unseen_messages = 0;
    }

    pub fn scroll_to_top(&mut self) {
        self.viewport.scroll_offset = 0;
        self.viewport.auto_scroll = false;
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
        let live_start = self
            .transcript
            .flushed_count
            .min(self.transcript.messages.len());
        let Some(index) = self.transcript.messages[live_start..]
            .iter()
            .rposition(|message| {
                matches!(
                    message,
                    ChatMessage::ToolCall { .. } | ChatMessage::Subagent { .. }
                )
            })
        else {
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
        self.transcript
            .messages
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
        if index < self.transcript.finalized_count {
            return true;
        }
        let is_last = index + 1 == self.transcript.messages.len();
        match &self.transcript.messages[index] {
            ChatMessage::ToolCall { status, .. } | ChatMessage::Subagent { status, .. } => {
                !matches!(status.as_str(), "running" | "receiving")
            }
            ChatMessage::Reasoning(_)
            | ChatMessage::Assistant(_)
            | ChatMessage::ProposedPlan(_) => turn_ended || !is_last,
            ChatMessage::AssistantChunk { .. }
            | ChatMessage::User(_)
            | ChatMessage::Image(_)
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
        let mut end = self.transcript.flushed_count;
        while end < self.transcript.messages.len() && self.message_is_settled(end, turn_ended) {
            end += 1;
        }
        end
    }

    pub fn remove_after_last_user(&mut self) {
        if let Some(index) = self
            .transcript
            .messages
            .iter()
            .rposition(|message| matches!(message, ChatMessage::User(_)))
        {
            self.truncate_messages(index);
        }
    }
}

#[cfg(test)]
#[path = "state_integration_tests.rs"]
mod tests;
