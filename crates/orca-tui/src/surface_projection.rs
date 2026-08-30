#![allow(dead_code)]

use std::collections::BTreeMap;

use orca_core::approval_types::ActionKind;
use orca_core::cost_types::UsageTotals;
use orca_core::goal_types::ThreadGoal;
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_core::task_types::{
    BackgroundTaskSummary, PendingToolCallSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    WorkflowPhaseTaskSummary, WorkflowTaskProgress,
};
use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowRunStatus};
use orca_runtime::surface::{
    AssistantChannel, AssistantPatch, ByteOffset, OperationPatch, OperationTerminal,
    SurfaceAssistantStream, SurfaceAssistantStreamState, SurfaceCommitBatch,
    SurfaceCompletedModelResponse, SurfaceCursor, SurfaceEvent, SurfaceFileChange, SurfaceGoal,
    SurfaceGoalPauseReason, SurfaceGoalReceiptState, SurfaceGoalState, SurfaceInputPresentation,
    SurfaceItem, SurfaceOperationFence, SurfaceOperationId, SurfaceReduceMode, SurfaceReduceResult,
    SurfaceReducerErrorCode, SurfaceReducerState, SurfaceStreamId, SurfaceTaskStatus,
    SurfaceToolResultKind, SurfaceUserInputState, SurfaceWorkflow, SurfaceWorkflowAgentStatus,
    SurfaceWorkflowStatus, ThreadPersistence, ToolPatch, UnixMillis,
};

use crate::protocol::{TuiEvent, TuiTaskLifecycle};
use crate::types::AppState;

/// Runtime-derived values that the TUI must keep in lockstep with the
/// authoritative surface reducer after each projected batch.
#[derive(Clone, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceProjectionState {
    pub(crate) cursor: SurfaceCursor,
    pub(crate) session_id: Option<String>,
    pub(crate) title: String,
    pub(crate) usage_revision: u64,
    pub(crate) usage: UsageTotals,
    pub(crate) context_revision: u64,
    pub(crate) context_used_tokens: usize,
    pub(crate) context_limit_tokens: usize,
    pub(crate) workflow_tasks: Vec<BackgroundTaskSummary>,
    pub(crate) current_goal: Option<ThreadGoal>,
    pub(crate) foreground_operation_id: Option<SurfaceOperationId>,
    pub(crate) recoverable_operation_id: Option<SurfaceOperationId>,
    pub(crate) goal_presentation: Option<GoalProjectionPresentation>,
    pub(crate) session_presentation: Option<SessionProjectionPresentation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionProjectionPresentation {
    Renamed,
    Forked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceSessionProjectionEffect {
    Renamed { title: String },
    Forked { title: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceSessionProjectionApply {
    Rejected,
    Accepted(Option<SurfaceSessionProjectionEffect>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceSessionProjectionState {
    session_id: Option<String>,
    title: Option<String>,
    cursor: Option<SurfaceCursor>,
    presented_cursor: Option<SurfaceCursor>,
}

impl SurfaceSessionProjectionState {
    pub(crate) fn accepts_reset(projection: &SurfaceProjectionState) -> bool {
        projection.session_presentation.is_none() || projection.session_id.is_some()
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub(crate) fn apply_projection(
        &mut self,
        projection: &SurfaceProjectionState,
    ) -> SurfaceSessionProjectionApply {
        if projection.session_presentation.is_some() && projection.session_id.is_none() {
            return SurfaceSessionProjectionApply::Rejected;
        }

        match self.cursor.as_ref() {
            None => {}
            Some(current)
                if current.thread_id != projection.cursor.thread_id
                    || current.incarnation != projection.cursor.incarnation =>
            {
                return SurfaceSessionProjectionApply::Rejected;
            }
            Some(current) if projection.cursor.next_seq < current.next_seq => {
                return SurfaceSessionProjectionApply::Rejected;
            }
            Some(current) if projection.cursor.next_seq == current.next_seq => {
                if current != &projection.cursor
                    || self.session_id != projection.session_id
                    || self.title.as_deref() != Some(projection.title.as_str())
                {
                    return SurfaceSessionProjectionApply::Rejected;
                }
            }
            Some(_) => {}
        }

        if self.cursor.as_ref() != Some(&projection.cursor) {
            self.session_id.clone_from(&projection.session_id);
            self.title = Some(projection.title.clone());
            self.cursor = Some(projection.cursor.clone());
        }

        let effect = projection.session_presentation.and_then(|presentation| {
            if self.presented_cursor.as_ref() == Some(&projection.cursor) {
                return None;
            }
            let title = self.title.clone()?;
            self.presented_cursor = Some(projection.cursor.clone());
            Some(match presentation {
                SessionProjectionPresentation::Renamed => {
                    SurfaceSessionProjectionEffect::Renamed { title }
                }
                SessionProjectionPresentation::Forked => {
                    SurfaceSessionProjectionEffect::Forked { title }
                }
            })
        });
        SurfaceSessionProjectionApply::Accepted(effect)
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.cursor.as_ref(), Some(&projection.cursor));
            debug_assert_eq!(self.session_id, projection.session_id);
            debug_assert_eq!(self.title.as_deref(), Some(projection.title.as_str()));
        }
        #[cfg(not(any(test, debug_assertions)))]
        let _ = projection;
    }

    #[cfg(test)]
    pub(crate) fn replace_for_test(&mut self, session_id: Option<String>, title: Option<String>) {
        self.session_id = session_id;
        self.title = title;
        self.cursor = None;
        self.presented_cursor = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalProjectionPresentation {
    Updated,
    Cleared,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceGoalProjectionEffect {
    Updated(ThreadGoal),
    Cleared,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceGoalProjectionState {
    current: Option<ThreadGoal>,
    cursor: Option<SurfaceCursor>,
    presented_cursor: Option<SurfaceCursor>,
}

impl SurfaceGoalProjectionState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn current(&self) -> Option<&ThreadGoal> {
        self.current.as_ref()
    }

    pub(crate) fn apply_projection(
        &mut self,
        projection: &SurfaceProjectionState,
    ) -> Option<SurfaceGoalProjectionEffect> {
        let accepts_projection = match self.cursor.as_ref() {
            None => true,
            Some(current)
                if current.thread_id != projection.cursor.thread_id
                    || current.incarnation != projection.cursor.incarnation =>
            {
                false
            }
            Some(current) if projection.cursor.next_seq < current.next_seq => false,
            Some(current) if projection.cursor.next_seq == current.next_seq => {
                self.current == projection.current_goal
            }
            Some(_) => true,
        };
        if !accepts_projection {
            return None;
        }

        if self.cursor.as_ref() != Some(&projection.cursor) {
            self.current = projection.current_goal.clone();
            self.cursor = Some(projection.cursor.clone());
        }

        let presentation = projection.goal_presentation?;
        if self.cursor.as_ref() != Some(&projection.cursor)
            || self.presented_cursor.as_ref() == Some(&projection.cursor)
        {
            return None;
        }
        let effect = match (presentation, self.current.as_ref()) {
            (GoalProjectionPresentation::Updated, Some(goal)) => {
                SurfaceGoalProjectionEffect::Updated(goal.clone())
            }
            (GoalProjectionPresentation::Cleared, None) => SurfaceGoalProjectionEffect::Cleared,
            _ => return None,
        };
        self.presented_cursor = Some(projection.cursor.clone());
        Some(effect)
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.cursor.as_ref(), Some(&projection.cursor));
            debug_assert_eq!(self.current, projection.current_goal);
        }
        #[cfg(not(any(test, debug_assertions)))]
        let _ = projection;
    }

    #[cfg(test)]
    pub(crate) fn replace_for_test(&mut self, goal: Option<ThreadGoal>) {
        self.current = goal;
        self.cursor = None;
        self.presented_cursor = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceOperationProjectionEffect {
    RecoveryPromptShown,
    RecoveryPromptCleared,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceOperationProjectionApply {
    Rejected,
    Accepted(Option<SurfaceOperationProjectionEffect>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceOperationProjectionState {
    foreground_operation_id: Option<SurfaceOperationId>,
    recoverable_operation_id: Option<SurfaceOperationId>,
    cursor: Option<SurfaceCursor>,
}

impl SurfaceOperationProjectionState {
    pub(crate) fn accepts_reset(projection: &SurfaceProjectionState) -> bool {
        projection
            .recoverable_operation_id
            .as_ref()
            .is_none_or(|recovery| projection.foreground_operation_id.as_ref() == Some(recovery))
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn foreground_operation_id(&self) -> Option<&SurfaceOperationId> {
        self.foreground_operation_id.as_ref()
    }

    pub(crate) fn recoverable_operation_id(&self) -> Option<&SurfaceOperationId> {
        self.recoverable_operation_id.as_ref()
    }

    pub(crate) fn apply_projection(
        &mut self,
        projection: &SurfaceProjectionState,
    ) -> SurfaceOperationProjectionApply {
        if !Self::accepts_reset(projection) {
            return SurfaceOperationProjectionApply::Rejected;
        }
        match self.cursor.as_ref() {
            None => {}
            Some(current)
                if current.thread_id != projection.cursor.thread_id
                    || current.incarnation != projection.cursor.incarnation
                    || projection.cursor.next_seq < current.next_seq =>
            {
                return SurfaceOperationProjectionApply::Rejected;
            }
            Some(current) if projection.cursor.next_seq == current.next_seq => {
                if current != &projection.cursor
                    || self.foreground_operation_id != projection.foreground_operation_id
                    || self.recoverable_operation_id != projection.recoverable_operation_id
                {
                    return SurfaceOperationProjectionApply::Rejected;
                }
                return SurfaceOperationProjectionApply::Accepted(None);
            }
            Some(_) => {}
        }

        let recovery_changed = self.recoverable_operation_id != projection.recoverable_operation_id;
        self.foreground_operation_id
            .clone_from(&projection.foreground_operation_id);
        self.recoverable_operation_id
            .clone_from(&projection.recoverable_operation_id);
        self.cursor = Some(projection.cursor.clone());
        let effect = recovery_changed.then(|| {
            if self.recoverable_operation_id.is_some() {
                SurfaceOperationProjectionEffect::RecoveryPromptShown
            } else {
                SurfaceOperationProjectionEffect::RecoveryPromptCleared
            }
        });
        SurfaceOperationProjectionApply::Accepted(effect)
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.cursor.as_ref(), Some(&projection.cursor));
            debug_assert_eq!(
                self.foreground_operation_id,
                projection.foreground_operation_id
            );
            debug_assert_eq!(
                self.recoverable_operation_id,
                projection.recoverable_operation_id
            );
        }
        #[cfg(not(any(test, debug_assertions)))]
        let _ = projection;
    }

    #[cfg(test)]
    pub(crate) fn replace_for_test(
        &mut self,
        foreground_operation_id: Option<SurfaceOperationId>,
        recoverable_operation_id: Option<SurfaceOperationId>,
    ) {
        self.foreground_operation_id = foreground_operation_id;
        self.recoverable_operation_id = recoverable_operation_id;
        self.cursor = None;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceWorkflowTaskProjectionState {
    tasks: Vec<BackgroundTaskSummary>,
    cursor: Option<SurfaceCursor>,
}

impl SurfaceWorkflowTaskProjectionState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn apply_projection(&mut self, projection: &SurfaceProjectionState) -> bool {
        match self.cursor.as_ref() {
            None => {}
            Some(current)
                if current.thread_id != projection.cursor.thread_id
                    || current.incarnation != projection.cursor.incarnation
                    || projection.cursor.next_seq < current.next_seq =>
            {
                return false;
            }
            Some(current) if projection.cursor.next_seq == current.next_seq => {
                return current == &projection.cursor && self.tasks == projection.workflow_tasks;
            }
            Some(_) => {}
        }

        self.tasks.clone_from(&projection.workflow_tasks);
        self.cursor = Some(projection.cursor.clone());
        true
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.cursor.as_ref(), Some(&projection.cursor));
            debug_assert_eq!(self.tasks, projection.workflow_tasks);
        }
        #[cfg(not(any(test, debug_assertions)))]
        let _ = projection;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceMetricsState {
    usage: UsageTotals,
    usage_revision: Option<u64>,
    context_revision: Option<u64>,
    context_used_tokens: usize,
    context_limit_tokens: usize,
}

impl SurfaceMetricsState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn rejects_usage_revision(&self, revision: u64) -> bool {
        self.usage_revision
            .is_some_and(|current| revision < current)
    }

    pub(crate) fn apply_projection(&mut self, projection: &SurfaceProjectionState) {
        self.usage = projection.usage.clone();
        self.usage_revision = Some(projection.usage_revision);
        if self
            .context_revision
            .is_none_or(|current| projection.context_revision > current)
        {
            self.context_revision = Some(projection.context_revision);
            self.context_used_tokens = projection.context_used_tokens;
            self.context_limit_tokens = projection.context_limit_tokens;
        }
    }

    pub(crate) fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    pub(crate) fn context_used_tokens(&self) -> usize {
        self.context_used_tokens
    }

    pub(crate) fn context_limit_tokens(&self) -> usize {
        self.context_limit_tokens
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.usage, projection.usage);
            debug_assert_eq!(self.usage_revision, Some(projection.usage_revision));
            debug_assert_eq!(self.context_revision, Some(projection.context_revision));
            debug_assert_eq!(self.context_used_tokens, projection.context_used_tokens);
            debug_assert_eq!(self.context_limit_tokens, projection.context_limit_tokens);
        }
        #[cfg(not(any(test, debug_assertions)))]
        let _ = projection;
    }
}

impl AppState {
    /// Returns the recorded session id from the latest accepted surface snapshot.
    pub fn current_session_id(&self) -> Option<&str> {
        self.surface_session.session_id()
    }

    /// Returns the title from the latest accepted surface snapshot.
    pub fn current_session_title(&self) -> Option<&str> {
        self.surface_session.title()
    }

    /// Returns usage from the latest committed runtime surface snapshot.
    pub fn usage(&self) -> &UsageTotals {
        self.surface_metrics.usage()
    }

    /// Returns context usage from the latest committed context revision.
    pub fn context_used_tokens(&self) -> usize {
        self.surface_metrics.context_used_tokens()
    }

    /// Returns the context limit from the latest committed context revision.
    pub fn context_limit_tokens(&self) -> usize {
        self.surface_metrics.context_limit_tokens()
    }

    /// Returns the latest Goal from an accepted runtime surface snapshot.
    pub fn current_goal(&self) -> Option<&ThreadGoal> {
        self.surface_goal.current()
    }

    /// Returns the foreground operation from the latest accepted surface snapshot.
    pub fn foreground_operation_id(&self) -> Option<&SurfaceOperationId> {
        self.surface_operation.foreground_operation_id()
    }

    /// Returns the recoverable operation from the latest accepted surface snapshot.
    pub fn recoverable_operation_id(&self) -> Option<&SurfaceOperationId> {
        self.surface_operation.recoverable_operation_id()
    }

    #[cfg(test)]
    pub(crate) fn replace_current_goal_for_test(&mut self, goal: Option<ThreadGoal>) {
        self.surface_goal.replace_for_test(goal);
    }

    #[cfg(test)]
    pub(crate) fn replace_surface_operation_for_test(
        &mut self,
        foreground_operation_id: Option<SurfaceOperationId>,
        recoverable_operation_id: Option<SurfaceOperationId>,
    ) {
        self.surface_operation
            .replace_for_test(foreground_operation_id, recoverable_operation_id);
    }

    #[cfg(test)]
    pub(crate) fn replace_session_identity_for_test(
        &mut self,
        session_id: Option<String>,
        title: Option<String>,
    ) {
        self.surface_session.replace_for_test(session_id, title);
    }
}

pub(crate) fn history_messages_from_surface_snapshot(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
) -> Vec<crate::transcript_state::ChatMessage> {
    history_messages_from_surface_items(&snapshot.items)
}

#[cfg(test)]
pub(crate) fn test_surface_cursor(next_seq: u64) -> SurfaceCursor {
    let mut thread_bytes = [1; 16];
    thread_bytes[6] = 0x71;
    thread_bytes[8] = 0x81;
    let mut incarnation_bytes = [2; 16];
    incarnation_bytes[6] = 0x72;
    incarnation_bytes[8] = 0x82;
    SurfaceCursor {
        thread_id: orca_runtime::surface::SurfaceThreadId::try_from_bytes(thread_bytes)
            .expect("test surface thread id"),
        incarnation: orca_runtime::surface::SurfaceIncarnation::try_from_bytes(incarnation_bytes)
            .expect("test surface incarnation"),
        next_seq: orca_runtime::surface::SequenceNumber::new(next_seq),
        source_revision: orca_runtime::surface::CursorSourceRevision::Recorded {
            durable_revision: orca_runtime::surface::DurableRevision::try_new(next_seq.max(1))
                .expect("test durable revision"),
        },
    }
}

fn history_messages_from_surface_items(
    items: &[SurfaceItem],
) -> Vec<crate::transcript_state::ChatMessage> {
    let mut messages = Vec::new();
    let mut index = 0;
    while index < items.len() {
        let Some(turn_id) = assistant_item_turn_id(&items[index]) else {
            if let Some(message) = history_message_from_surface_item(&items[index]) {
                messages.push(message);
            }
            index += 1;
            continue;
        };
        let start = index;
        while index < items.len()
            && assistant_item_turn_id(&items[index]).is_some_and(|candidate| candidate == turn_id)
        {
            index += 1;
        }
        let assistant_items = &items[start..index];
        for item in assistant_items
            .iter()
            .filter(|item| matches!(item, SurfaceItem::AssistantReasoning { .. }))
            .chain(
                assistant_items
                    .iter()
                    .filter(|item| matches!(item, SurfaceItem::AssistantMessage { .. })),
            )
            .chain(
                assistant_items
                    .iter()
                    .filter(|item| matches!(item, SurfaceItem::AssistantPlan { .. })),
            )
        {
            if let Some(message) = history_message_from_surface_item(item) {
                messages.push(message);
            }
        }
    }
    messages
}

fn assistant_item_turn_id(item: &SurfaceItem) -> Option<&orca_runtime::surface::SurfaceTurnId> {
    match item {
        SurfaceItem::AssistantMessage { turn_id, .. }
        | SurfaceItem::AssistantReasoning { turn_id, .. }
        | SurfaceItem::AssistantPlan { turn_id, .. } => Some(turn_id),
        SurfaceItem::UserMessage { .. }
        | SurfaceItem::SystemMessage { .. }
        | SurfaceItem::ToolResultMessage { .. } => None,
    }
}

fn history_message_from_surface_item(
    item: &SurfaceItem,
) -> Option<crate::transcript_state::ChatMessage> {
    match item {
        SurfaceItem::UserMessage { input, .. } => match input {
            SurfaceUserInputState::Pending { presentation, .. }
            | SurfaceUserInputState::ResolutionFailed { presentation, .. } => {
                visible_input_text(presentation).map(crate::transcript_state::ChatMessage::User)
            }
            SurfaceUserInputState::Resolved { fact } => match fact {
                orca_runtime::surface::SurfaceResolvedInputFact::Replayable { input, .. } => {
                    Some(crate::transcript_state::ChatMessage::User(
                        input.canonical_text.as_str().to_string(),
                    ))
                }
                orca_runtime::surface::SurfaceResolvedInputFact::NonReplayable {
                    presentation,
                    ..
                } => {
                    visible_input_text(presentation).map(crate::transcript_state::ChatMessage::User)
                }
            },
        },
        SurfaceItem::SystemMessage { content, .. }
            if content
                .as_str()
                .starts_with(orca_core::conversation::IMAGE_ANALYSIS_MESSAGE_PREFIX) =>
        {
            None
        }
        SurfaceItem::SystemMessage { content, .. } => Some(
            crate::transcript_state::ChatMessage::System(content.as_str().to_string()),
        ),
        SurfaceItem::AssistantMessage { text, .. } => (!text.as_str().trim().is_empty())
            .then(|| crate::transcript_state::ChatMessage::Assistant(text.as_str().to_string())),
        SurfaceItem::AssistantReasoning {
            content, summary, ..
        } => {
            let text = if content.as_str().trim().is_empty() {
                summary.as_str()
            } else {
                content.as_str()
            };
            (!text.trim().is_empty())
                .then(|| crate::transcript_state::ChatMessage::Reasoning(text.to_string()))
        }
        SurfaceItem::AssistantPlan { text, .. } => (!text.as_str().trim().is_empty())
            .then(|| crate::transcript_state::ChatMessage::ProposedPlan(text.as_str().to_string())),
        SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } => {
            let output = (!content.as_str().is_empty()).then(|| content.as_str().to_string());
            Some(crate::transcript_state::ChatMessage::ToolCall {
                id: tool_call_id.as_str().to_string(),
                name: format!("tool:{}", tool_call_id.as_str()),
                target: None,
                status: tool_result_status(terminal.kind).to_string(),
                output,
                diff: None,
                kind: None,
                expanded: false,
            })
        }
    }
}

fn visible_input_text(presentation: &SurfaceInputPresentation) -> Option<String> {
    match presentation {
        SurfaceInputPresentation::Visible { text } => Some(text.as_str().to_string()),
        SurfaceInputPresentation::Redacted => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceProjectionError {
    CursorGap {
        expected: SurfaceCursor,
        observed: SurfaceCursor,
    },
    UnknownAssistantStream {
        stream_id: SurfaceStreamId,
    },
    ReducerRejected {
        code: SurfaceReducerErrorCode,
    },
    MissingReducerSnapshot,
    InvalidDeliveryWatermark {
        stream_id: SurfaceStreamId,
        offset: ByteOffset,
    },
}

pub(crate) type TuiStreamDeliveryWatermark = BTreeMap<SurfaceStreamId, ByteOffset>;

impl SurfaceProjectionState {
    pub(crate) fn from_surface_snapshot(snapshot: &orca_runtime::surface::SurfaceSnapshot) -> Self {
        Self {
            cursor: snapshot.cursor.clone(),
            session_id: matches!(
                snapshot.thread.persistence,
                ThreadPersistence::RecordedCatalogued
            )
            .then(|| surface_thread_id_text(&snapshot.thread.thread_id)),
            title: snapshot.thread.title.as_str().to_string(),
            usage_revision: snapshot.usage.revision.get(),
            usage: core_usage_totals(&snapshot.usage.thread_total),
            context_revision: snapshot.context.revision.get(),
            context_used_tokens: usize::try_from(snapshot.context.used_tokens)
                .unwrap_or(usize::MAX),
            context_limit_tokens: usize::try_from(snapshot.context.limit_tokens)
                .unwrap_or(usize::MAX),
            workflow_tasks: workflow_task_summaries(snapshot),
            current_goal: snapshot.goal.as_ref().map(|goal| {
                thread_goal_from_surface(
                    goal,
                    snapshot.thread.created_at,
                    snapshot.thread.updated_at,
                )
            }),
            foreground_operation_id: snapshot
                .foreground_operation
                .as_ref()
                .map(|operation| operation.operation_id.clone()),
            recoverable_operation_id: snapshot
                .recoverable_user_operation()
                .map(|operation| operation.operation_id().clone()),
            goal_presentation: None,
            session_presentation: None,
        }
    }

    pub(crate) fn with_goal_presentation(
        mut self,
        presentation: GoalProjectionPresentation,
    ) -> Self {
        self.goal_presentation = Some(presentation);
        self
    }

    pub(crate) fn with_session_presentation(
        mut self,
        presentation: SessionProjectionPresentation,
    ) -> Self {
        self.session_presentation = Some(presentation);
        self
    }
}

pub(crate) struct TuiSurfaceProjection {
    cursor: SurfaceCursor,
    assistant_streams: BTreeMap<SurfaceStreamId, SurfaceAssistantStream>,
    assistant_stream_order: Vec<SurfaceStreamId>,
    completed_items: Vec<SurfaceItem>,
    operation_turn_ids: BTreeMap<SurfaceOperationId, Vec<orca_runtime::surface::SurfaceTurnId>>,
    focused_operation: Option<SurfaceOperationId>,
    pending_turn_started: Option<TuiTaskLifecycle>,
    goal: Option<SurfaceGoal>,
    reducer_state: Option<SurfaceReducerState>,
}

impl TuiSurfaceProjection {
    pub(crate) fn from_snapshot(cursor: SurfaceCursor, streams: &[SurfaceAssistantStream]) -> Self {
        Self {
            cursor,
            assistant_stream_order: streams
                .iter()
                .map(|stream| stream.stream_id.clone())
                .collect(),
            assistant_streams: streams
                .iter()
                .map(|stream| (stream.stream_id.clone(), stream.clone()))
                .collect(),
            completed_items: Vec::new(),
            operation_turn_ids: BTreeMap::new(),
            focused_operation: None,
            pending_turn_started: None,
            goal: None,
            reducer_state: None,
        }
    }

    pub(crate) fn from_surface_snapshot(snapshot: &orca_runtime::surface::SurfaceSnapshot) -> Self {
        let mut projection = Self::from_snapshot(
            snapshot.cursor.clone(),
            snapshot.assistant_streams.as_slice(),
        );
        projection.focused_operation = snapshot
            .foreground_operation
            .as_ref()
            .map(|operation| operation.operation_id.clone());
        projection.pending_turn_started = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.terminal.is_none())
            .and_then(|operation| operation.agent_loop_turns.last())
            .map(|turn| TuiTaskLifecycle {
                id: turn.task_id.as_str().to_string(),
                kind: "agent".to_string(),
                status: "running".to_string(),
                turn: turn.ordinal,
            });
        projection.goal = snapshot.goal.clone();
        projection.completed_items = snapshot.items.clone();
        projection.operation_turn_ids = snapshot
            .operation_history
            .iter()
            .map(|operation| {
                let mut turn_ids = operation
                    .generations
                    .iter()
                    .map(|generation| generation.logical_turn_id.clone())
                    .chain(
                        operation
                            .agent_loop_turns
                            .iter()
                            .map(|turn| turn.turn_id.clone()),
                    )
                    .collect::<Vec<_>>();
                if let Some(turn_id) = operation.initial_logical_turn_id.clone() {
                    turn_ids.push(turn_id);
                }
                turn_ids.sort();
                turn_ids.dedup();
                (operation.operation_id.clone(), turn_ids)
            })
            .collect();
        projection.reducer_state = Some(SurfaceReducerState::new(snapshot.clone()));
        projection
    }

    pub(crate) fn hydrate_open_streams(&mut self) -> Vec<TuiEvent> {
        let mut projected = self
            .pending_turn_started
            .take()
            .map(|task| {
                vec![TuiEvent::TurnStarted {
                    turn: task.turn,
                    task: Some(task),
                }]
            })
            .unwrap_or_default();
        projected.extend(
            self.assistant_stream_order
                .iter()
                .filter_map(|stream_id| self.assistant_streams.get(stream_id))
                .filter(|stream| {
                    stream.state == SurfaceAssistantStreamState::Open
                        && !stream.text.as_str().is_empty()
                })
                .map(|stream| match stream.channel {
                    AssistantChannel::Message => {
                        TuiEvent::MessageDelta(stream.text.as_str().to_string())
                    }
                    AssistantChannel::Reasoning => {
                        TuiEvent::ReasoningDelta(stream.text.as_str().to_string())
                    }
                    AssistantChannel::Plan => TuiEvent::Notice(stream.text.as_str().to_string()),
                })
                .collect::<Vec<_>>(),
        );
        projected
    }

    pub(crate) fn delivery_watermark(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> TuiStreamDeliveryWatermark {
        self.assistant_streams
            .values()
            .filter(|stream| {
                &stream.fence.operation_id == operation_id
                    && stream.state != SurfaceAssistantStreamState::Discarded
            })
            .map(|stream| (stream.stream_id.clone(), stream.next_offset))
            .collect()
    }

    pub(crate) fn hydrate_after_delivery_watermark(
        &self,
        operation_id: &SurfaceOperationId,
        watermark: &TuiStreamDeliveryWatermark,
    ) -> Result<Vec<TuiEvent>, SurfaceProjectionError> {
        let mut projected = self
            .assistant_stream_order
            .iter()
            .filter_map(|stream_id| self.assistant_streams.get(stream_id))
            .filter(|stream| {
                &stream.fence.operation_id == operation_id
                    && stream.state != SurfaceAssistantStreamState::Discarded
            })
            .filter_map(|stream| {
                let offset = watermark
                    .get(&stream.stream_id)
                    .copied()
                    .unwrap_or_else(|| ByteOffset::new(0));
                let Ok(offset_usize) = usize::try_from(offset.get()) else {
                    return Some(Err(SurfaceProjectionError::InvalidDeliveryWatermark {
                        stream_id: stream.stream_id.clone(),
                        offset,
                    }));
                };
                let text = stream.text.as_str();
                if offset_usize > text.len() || !text.is_char_boundary(offset_usize) {
                    return Some(Err(SurfaceProjectionError::InvalidDeliveryWatermark {
                        stream_id: stream.stream_id.clone(),
                        offset,
                    }));
                }
                let suffix = &text[offset_usize..];
                if suffix.is_empty() {
                    return None;
                }
                Some(Ok(match stream.channel {
                    AssistantChannel::Message => TuiEvent::MessageDelta(suffix.to_string()),
                    AssistantChannel::Reasoning => TuiEvent::ReasoningDelta(suffix.to_string()),
                    AssistantChannel::Plan => TuiEvent::Notice(suffix.to_string()),
                }))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let Some(turn_ids) = self.operation_turn_ids.get(operation_id) else {
            return Ok(projected);
        };
        let streamed_item_ids = self
            .assistant_streams
            .values()
            .filter(|stream| &stream.fence.operation_id == operation_id)
            .map(|stream| &stream.item_id)
            .collect::<Vec<_>>();
        projected.extend(self.completed_items.iter().filter_map(|item| match item {
            SurfaceItem::AssistantMessage {
                id, turn_id, text, ..
            } if turn_ids.contains(turn_id)
                && !streamed_item_ids.iter().any(|streamed| *streamed == id)
                && !text.as_str().is_empty() =>
            {
                Some(TuiEvent::MessageDelta(text.as_str().to_string()))
            }
            SurfaceItem::AssistantReasoning {
                id,
                turn_id,
                summary,
                content,
                ..
            } if turn_ids.contains(turn_id)
                && !streamed_item_ids.iter().any(|streamed| *streamed == id) =>
            {
                let text = if content.as_str().is_empty() {
                    summary.as_str()
                } else {
                    content.as_str()
                };
                (!text.is_empty()).then(|| TuiEvent::ReasoningDelta(text.to_string()))
            }
            SurfaceItem::AssistantPlan {
                id, turn_id, text, ..
            } if turn_ids.contains(turn_id)
                && !streamed_item_ids.iter().any(|streamed| *streamed == id)
                && !text.as_str().is_empty() =>
            {
                Some(TuiEvent::Notice(text.as_str().to_string()))
            }
            _ => None,
        }));
        for turn_id in turn_ids {
            let discarded_item_ids = self
                .assistant_streams
                .values()
                .filter(|stream| {
                    &stream.fence.operation_id == operation_id
                        && &stream.turn_id == turn_id
                        && stream.state == SurfaceAssistantStreamState::Discarded
                })
                .map(|stream| &stream.item_id)
                .collect::<Vec<_>>();
            if discarded_item_ids.is_empty() {
                continue;
            }
            let message = self.completed_items.iter().find_map(|item| match item {
                SurfaceItem::AssistantMessage { id, text, .. }
                    if discarded_item_ids.iter().any(|discarded| *discarded == id) =>
                {
                    Some(text.as_str().to_string())
                }
                _ => None,
            });
            let reasoning = self.completed_items.iter().find_map(|item| match item {
                SurfaceItem::AssistantReasoning {
                    id,
                    summary,
                    content,
                    ..
                } if discarded_item_ids.iter().any(|discarded| *discarded == id) => Some(
                    if content.as_str().is_empty() {
                        summary.as_str()
                    } else {
                        content.as_str()
                    }
                    .to_string(),
                ),
                _ => None,
            });
            if message.is_some() || reasoning.is_some() {
                projected.push(TuiEvent::AssistantResponseCompleted(message, reasoning));
            }
            projected.extend(self.completed_items.iter().filter_map(|item| match item {
                SurfaceItem::AssistantPlan { id, text, .. }
                    if discarded_item_ids.iter().any(|discarded| *discarded == id)
                        && !text.as_str().is_empty() =>
                {
                    Some(TuiEvent::Notice(text.as_str().to_string()))
                }
                _ => None,
            }));
        }
        Ok(projected)
    }

    #[allow(dead_code)]
    pub(crate) fn focus_operation(&mut self, operation_id: SurfaceOperationId) {
        self.focused_operation = Some(operation_id);
    }

    pub(crate) fn cursor(&self) -> &SurfaceCursor {
        &self.cursor
    }

    pub(crate) fn reduce_typed_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<Vec<TuiEvent>, SurfaceProjectionError> {
        if batch.cursor_before != self.cursor {
            return Err(SurfaceProjectionError::CursorGap {
                expected: self.cursor.clone(),
                observed: batch.cursor_before.clone(),
            });
        }
        let next_reducer_state = match self.reducer_state.as_ref() {
            Some(state) => {
                match orca_runtime::surface::reduce_batch(SurfaceReduceMode::Live, state, batch) {
                    SurfaceReduceResult::Applied { state } => Some(state),
                    SurfaceReduceResult::AlreadyApplied { .. } => Some(state.clone()),
                    SurfaceReduceResult::Rejected { error } => {
                        return Err(SurfaceProjectionError::ReducerRejected { code: error.code });
                    }
                }
            }
            None => None,
        };

        let mut assistant_streams = self.assistant_streams.clone();
        let mut assistant_stream_order = self.assistant_stream_order.clone();
        let mut focused_operation = self.focused_operation.clone();
        let mut goal = self.goal.clone();
        let mut projected = Vec::new();
        for envelope in batch.events.as_slice() {
            match &envelope.event {
                SurfaceEvent::Assistant(AssistantPatch::StreamOpened { stream }) => {
                    if !assistant_streams.contains_key(&stream.stream_id) {
                        assistant_stream_order.push(stream.stream_id.clone());
                    }
                    assistant_streams
                        .entry(stream.stream_id.clone())
                        .and_modify(|current| *current = stream.clone())
                        .or_insert_with(|| stream.clone());
                }
                SurfaceEvent::Assistant(AssistantPatch::Delta {
                    stream_id,
                    offset,
                    text,
                }) => {
                    let stream = assistant_streams.get_mut(stream_id).ok_or_else(|| {
                        SurfaceProjectionError::UnknownAssistantStream {
                            stream_id: stream_id.clone(),
                        }
                    })?;
                    if stream.state != SurfaceAssistantStreamState::Open
                        || stream.next_offset != *offset
                    {
                        return Err(SurfaceProjectionError::UnknownAssistantStream {
                            stream_id: stream_id.clone(),
                        });
                    }
                    stream.text = orca_runtime::surface::DisplayText::new(format!(
                        "{}{}",
                        stream.text.as_str(),
                        text.as_str()
                    ));
                    stream.next_offset = orca_runtime::surface::ByteOffset::new(
                        offset.get().saturating_add(text.as_str().len() as u64),
                    );
                    match stream.channel {
                        AssistantChannel::Message => {
                            projected.push(TuiEvent::MessageDelta(text.as_str().to_string()));
                        }
                        AssistantChannel::Reasoning => {
                            projected.push(TuiEvent::ReasoningDelta(text.as_str().to_string()));
                        }
                        AssistantChannel::Plan => {}
                    }
                }
                SurfaceEvent::Assistant(AssistantPatch::StreamDiscarded { stream_id, .. }) => {
                    if let Some(stream) = assistant_streams.get_mut(stream_id) {
                        if stream.state == SurfaceAssistantStreamState::Open {
                            projected.push(TuiEvent::AssistantAttemptDiscarded);
                        }
                        stream.state = SurfaceAssistantStreamState::Discarded;
                    }
                }
                SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted { response }) => {
                    let discarded_attempt = projected
                        .iter()
                        .any(|event| matches!(event, TuiEvent::AssistantAttemptDiscarded));
                    let response_matches_streams =
                        response_matches_streamed_items(response, assistant_streams.values());
                    for stream in assistant_streams.values_mut().filter(|stream| {
                        stream.turn_id == response.turn_id
                            && stream.state == SurfaceAssistantStreamState::Open
                    }) {
                        stream.state = SurfaceAssistantStreamState::Completed;
                    }
                    if discarded_attempt || !response_matches_streams {
                        projected.push(response_completed_event(response));
                    }
                }
                SurfaceEvent::Tool(ToolPatch::Requested { request }) => {
                    projected.push(TuiEvent::ToolRequested {
                        id: request.tool_call_id.as_str().to_string(),
                        name: request.name.as_str().to_string(),
                        target: request
                            .target
                            .as_ref()
                            .map(|target| target.as_str().to_string()),
                    });
                }
                SurfaceEvent::Tool(ToolPatch::ArgumentsProgress {
                    tool_call_id,
                    arguments_bytes,
                }) => projected.push(TuiEvent::ToolCallProgress {
                    id: tool_call_id.as_str().to_string(),
                    name: None,
                    arguments_bytes: usize::try_from(arguments_bytes.get()).unwrap_or(usize::MAX),
                }),
                SurfaceEvent::Tool(ToolPatch::OutputDelta {
                    tool_call_id,
                    chunk,
                    ..
                }) => projected.push(TuiEvent::ToolOutputDelta {
                    id: tool_call_id.as_str().to_string(),
                    chunk: chunk.as_str().to_string(),
                }),
                SurfaceEvent::Tool(ToolPatch::Completed { result }) => {
                    let (diff, kind) = match &result.file_change {
                        Some(SurfaceFileChange::UnifiedDiff { text, .. }) => (
                            Some(text.as_str().to_string()),
                            Some("file_change".to_string()),
                        ),
                        Some(SurfaceFileChange::PreviewOmitted { .. }) => {
                            (None, Some("file_change".to_string()))
                        }
                        None => (None, None),
                    };
                    projected.push(TuiEvent::ToolCompleted {
                        id: result.tool_call_id.as_str().to_string(),
                        name: result.name.as_str().to_string(),
                        status: tool_result_status(result.terminal.kind).to_string(),
                        output: result
                            .output
                            .as_ref()
                            .or(result.error.as_ref())
                            .map(|text| text.as_str().to_string())
                            .unwrap_or_default(),
                        diff,
                        kind,
                    });
                }
                SurfaceEvent::Plan(plan) => projected.push(TuiEvent::PlanUpdated {
                    explanation: plan
                        .explanation
                        .as_ref()
                        .map(|value| value.as_str().to_string()),
                    plan: plan
                        .items
                        .iter()
                        .map(|item| PlanItem {
                            step: item.step.as_str().to_string(),
                            status: match item.status {
                                orca_runtime::surface::SurfacePlanStatus::Pending => {
                                    PlanStatus::Pending
                                }
                                orca_runtime::surface::SurfacePlanStatus::InProgress => {
                                    PlanStatus::InProgress
                                }
                                orca_runtime::surface::SurfacePlanStatus::Completed => {
                                    PlanStatus::Completed
                                }
                            },
                        })
                        .collect(),
                }),
                SurfaceEvent::Usage(_) => {}
                SurfaceEvent::Context(context) => match &context.compaction {
                    orca_runtime::surface::CompactionState::Running { .. } => {
                        projected.push(TuiEvent::CompactionStarted);
                    }
                    orca_runtime::surface::CompactionState::Completed {
                        reason,
                        strategy,
                        before_messages,
                        after_messages,
                        collapsed_messages,
                        status_text,
                        ..
                    } => {
                        projected.push(TuiEvent::Compacted {
                            before_messages: usize::try_from(*before_messages)
                                .unwrap_or(usize::MAX),
                            after_messages: usize::try_from(*after_messages).unwrap_or(usize::MAX),
                            reason: match reason {
                                orca_runtime::surface::CompactionReason::Manual => {
                                    "manual".to_string()
                                }
                                orca_runtime::surface::CompactionReason::Automatic => {
                                    "automatic".to_string()
                                }
                            },
                            strategy: strategy.as_str().to_string(),
                            collapsed_messages: usize::try_from(*collapsed_messages)
                                .unwrap_or(usize::MAX),
                            status_text: status_text.as_str().to_string(),
                        });
                    }
                    orca_runtime::surface::CompactionState::Idle => {}
                },
                SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn })
                    if focused_operation.as_ref() == Some(&turn.fence.operation_id) =>
                {
                    projected.push(TuiEvent::TurnStarted {
                        turn: turn.ordinal,
                        task: Some(TuiTaskLifecycle {
                            id: turn.task_id.as_str().to_string(),
                            kind: "agent".to_string(),
                            status: "running".to_string(),
                            turn: turn.ordinal,
                        }),
                    });
                }
                SurfaceEvent::Operation(OperationPatch::Terminal { record })
                    if focused_operation.as_ref() == Some(&record.operation_id) =>
                {
                    if let Some(status) = operation_terminal_status(&record.terminal) {
                        projected.push(TuiEvent::SessionCompleted {
                            status: status.to_string(),
                        });
                    }
                    focused_operation = None;
                }
                SurfaceEvent::Goal(goal_patch) => {
                    let previous_state = goal.as_ref().map(|goal| goal.state.clone());
                    match &goal_patch.patch {
                        orca_runtime::surface::GoalPatch::Created { goal: created }
                        | orca_runtime::surface::GoalPatch::Edited { goal: created, .. } => {
                            goal = Some(created.clone());
                        }
                        orca_runtime::surface::GoalPatch::Removed { .. } => {
                            goal = None;
                            continue;
                        }
                        patch => {
                            let Some(current) = goal.as_mut() else {
                                continue;
                            };
                            current.goal_revision = goal_patch.receipt.goal_revision;
                            current.objective_revision = goal_patch.receipt.objective_revision;
                            current.catalog_revision = goal_patch.receipt.catalog_revision;
                            current.goal_owner_epoch = goal_patch.receipt.goal_owner_epoch;
                            match &goal_patch.receipt.row_state {
                                SurfaceGoalReceiptState::Present { state, current_run } => {
                                    current.state = state.clone();
                                    current.current_run = current_run.clone();
                                }
                                SurfaceGoalReceiptState::Removed { .. } => {
                                    goal = None;
                                    continue;
                                }
                            }
                            match patch {
                                orca_runtime::surface::GoalPatch::OuterTurnFinished {
                                    usage,
                                    ..
                                }
                                | orca_runtime::surface::GoalPatch::Completed { usage, .. } => {
                                    current.usage = usage.clone()
                                }
                                orca_runtime::surface::GoalPatch::Transitioned {
                                    transition,
                                    ..
                                } => current.last_transition = Some(transition.clone()),
                                _ => {}
                            }
                        }
                    }
                    if let Some(current) = goal.as_ref() {
                        if !matches!(
                            previous_state,
                            Some(SurfaceGoalState::Paused {
                                reason: SurfaceGoalPauseReason::NoProgress,
                                ..
                            })
                        ) && matches!(
                            current.state,
                            SurfaceGoalState::Paused {
                                reason: SurfaceGoalPauseReason::NoProgress,
                                ..
                            }
                        ) {
                            projected.push(TuiEvent::Notice(
                                "Goal paused because the last turns made no measurable progress. Use /goal resume to continue."
                                    .to_string(),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        self.assistant_streams = assistant_streams;
        self.assistant_stream_order = assistant_stream_order;
        self.focused_operation = focused_operation;
        self.goal = goal;
        self.reducer_state = next_reducer_state;
        self.cursor = batch.cursor_after.clone();
        Ok(projected)
    }

    /// Projects one committed batch and appends the canonical reducer snapshot
    /// used to reconcile TUI-owned derived state at the batch boundary.
    pub(crate) fn project_typed_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<Vec<TuiEvent>, SurfaceProjectionError> {
        if self.reducer_state.is_none() {
            return Err(SurfaceProjectionError::MissingReducerSnapshot);
        }
        let has_goal_patch = batch
            .events
            .as_slice()
            .iter()
            .any(|event| matches!(&event.event, SurfaceEvent::Goal(_)));
        let needs_projection_snapshot = batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                SurfaceEvent::Operation(_)
                    | SurfaceEvent::Usage(_)
                    | SurfaceEvent::Context(_)
                    | SurfaceEvent::Task(_)
                    | SurfaceEvent::Workflow(_)
                    | SurfaceEvent::Subagent(_)
                    | SurfaceEvent::Goal(_)
                    | SurfaceEvent::Session(_)
            )
        });
        let mut projected = self.reduce_typed_batch(batch)?;
        if !needs_projection_snapshot {
            return Ok(projected);
        }
        let Some(mut state) = self
            .reducer_state
            .as_ref()
            .map(|state| SurfaceProjectionState::from_surface_snapshot(state.snapshot()))
        else {
            return Err(SurfaceProjectionError::MissingReducerSnapshot);
        };
        if has_goal_patch {
            let presentation = if state.current_goal.is_some() {
                GoalProjectionPresentation::Updated
            } else {
                GoalProjectionPresentation::Cleared
            };
            state = state.with_goal_presentation(presentation);
        }
        projected.push(TuiEvent::SurfaceProjectionSynced(Box::new(state)));
        Ok(projected)
    }

    pub(crate) fn terminal_workflow_notification(
        &self,
        workflow_run_id: &orca_runtime::surface::SurfaceWorkflowRunId,
    ) -> Option<TuiEvent> {
        let workflow = self
            .reducer_state
            .as_ref()?
            .snapshot()
            .workflows
            .iter()
            .find(|workflow| &workflow.workflow_run_id == workflow_run_id)?;
        workflow_terminal_notification(workflow)
    }

    pub(crate) fn active_generation_fence(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> Option<SurfaceOperationFence> {
        let snapshot = self.reducer_state.as_ref()?.snapshot();
        snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| &operation.operation_id == operation_id)
            .and_then(|operation| operation.generations.last())
            .map(|generation| generation.fence.clone())
            .or_else(|| {
                snapshot
                    .background_operations
                    .iter()
                    .find(|operation| &operation.operation_id == operation_id)
                    .map(|operation| operation.fence.operation_fence.clone())
            })
    }

    pub(crate) fn operation_is_runtime_backgrounded(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> bool {
        self.reducer_state.as_ref().is_some_and(|state| {
            state
                .snapshot()
                .background_operations
                .iter()
                .any(|operation| &operation.operation_id == operation_id)
        })
    }

    pub(crate) fn terminal_status_for_operation(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> Option<&'static str> {
        let snapshot = self.reducer_state.as_ref()?.snapshot();
        snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)
            .and_then(|operation| operation.terminal.as_ref())
            .and_then(|record| operation_terminal_status(&record.terminal))
    }

    pub(crate) fn background_task_summary_for_operation(
        &self,
        operation_id: &SurfaceOperationId,
    ) -> Option<BackgroundTaskSummary> {
        background_task_summary_for_operation(self.reducer_state.as_ref()?.snapshot(), operation_id)
    }
}

pub(crate) fn workflow_task_summaries(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
) -> Vec<BackgroundTaskSummary> {
    snapshot
        .tasks
        .iter()
        .map(|task| {
            let subagent = task.subagent_id.as_ref().and_then(|subagent_id| {
                snapshot
                    .subagents
                    .iter()
                    .find(|subagent| &subagent.subagent_id == subagent_id)
            });
            let workflow = task.workflow_run_id.as_ref().and_then(|run_id| {
                snapshot
                    .workflows
                    .iter()
                    .find(|workflow| &workflow.workflow_run_id == run_id)
            });
            let pending_tool_call = pending_tool_call_for_task(snapshot, task);
            BackgroundTaskSummary {
                id: task.task_id.as_str().to_string(),
                parent_task_id: task
                    .parent_task_id
                    .as_ref()
                    .map(|parent| parent.as_str().to_string()),
                task_type: task_type(task.task_type),
                status: task_status(task.status),
                is_backgrounded: task.backgrounded,
                description: task.description.as_str().to_string(),
                created_at_ms: task.created_at.get(),
                started_at_ms: task.started_at.map(UnixMillis::get),
                completed_at_ms: task.completed_at.map(UnixMillis::get),
                command: None,
                agent_type: None,
                server: None,
                tool: pending_tool_call
                    .as_ref()
                    .map(|tool| tool.name.clone())
                    .or_else(|| workflow.map(|_| "workflow".to_string())),
                pending_tool_call,
                name: workflow.map(|workflow| workflow.name.as_str().to_string()),
                workflow_run_id: task
                    .workflow_run_id
                    .as_ref()
                    .map(|run_id| run_id.as_str().to_string()),
                phase_count: workflow.map(|workflow| workflow.phases.len()),
                workflow_progress: workflow.map(workflow_progress),
                workflow_phases: workflow
                    .map(|workflow| {
                        workflow
                            .phases
                            .iter()
                            .map(|phase| WorkflowPhaseTaskSummary {
                                name: phase.name.as_str().to_string(),
                                status: workflow_status(phase.status),
                                agent_count: phase.agent_count,
                                error: phase.error.as_ref().map(|error| error.as_str().to_string()),
                                fallback: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                workflow_agents: workflow
                    .map(|workflow| {
                        workflow
                            .agents
                            .iter()
                            .map(|agent| WorkflowAgentTaskSummary {
                                call_id: agent.agent_id.as_str().to_string(),
                                call_path: agent.agent_id.as_str().to_string(),
                                team: None,
                                status: workflow_agent_status(agent.status),
                                attempt: agent.attempt,
                                max_attempts: agent.attempt.max(1),
                                previous_errors: Vec::new(),
                                error: agent.error.as_ref().map(|error| error.as_str().to_string()),
                                transcript_path: None,
                                started_at_ms: None,
                                completed_at_ms: None,
                                usage: agent.usage.as_ref().map(core_usage_totals),
                                continuation: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                workflow_script_path: None,
                workflow_launch_input: None,
                workflow_final_summary: workflow.and_then(|workflow| {
                    workflow
                        .result
                        .as_ref()
                        .map(|result| result.content.as_str().to_string())
                }),
                workflow_failure_count: workflow
                    .map(|workflow| {
                        workflow
                            .agents
                            .iter()
                            .filter(|agent| agent.status == SurfaceWorkflowAgentStatus::Failed)
                            .count() as u32
                    })
                    .unwrap_or_default(),
                usage: task.usage.as_ref().map(core_usage_totals),
                subagent_current_activity: subagent
                    .and_then(|subagent| subagent.activity.as_ref())
                    .map(|activity| activity.as_str().to_string()),
                subagent_turn: subagent.and_then(|subagent| subagent.turn),
                last_activity_at_ms: task.completed_at.or(task.started_at).map(UnixMillis::get),
                continuation: None,
                result: task.result.as_ref().map(|value| value.as_str().to_string()),
                error: task.error.as_ref().map(|value| value.as_str().to_string()),
                retry_count: task.retry_count,
                output_truncated: task.output_truncated,
                publication_revision: Some(task.revision.get()),
            }
        })
        .collect()
}

pub(crate) fn background_task_summary_for_operation(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
    operation_id: &SurfaceOperationId,
) -> Option<BackgroundTaskSummary> {
    let task_id = snapshot
        .tasks
        .iter()
        .find(|task| {
            task.task_type == orca_runtime::surface::SurfaceTaskType::MainSession
                && task.parent_operation.as_ref() == Some(operation_id)
        })?
        .task_id
        .as_str();
    workflow_task_summaries(snapshot)
        .into_iter()
        .find(|task| task.id == task_id)
}

fn workflow_progress(workflow: &SurfaceWorkflow) -> WorkflowTaskProgress {
    WorkflowTaskProgress {
        total_agents: workflow.agents.len() as u32,
        running_agents: workflow
            .agents
            .iter()
            .filter(|agent| agent.status == SurfaceWorkflowAgentStatus::Running)
            .count() as u32,
        completed_agents: workflow
            .agents
            .iter()
            .filter(|agent| {
                matches!(
                    agent.status,
                    SurfaceWorkflowAgentStatus::Completed | SurfaceWorkflowAgentStatus::Cached
                )
            })
            .count() as u32,
        failed_agents: workflow
            .agents
            .iter()
            .filter(|agent| agent.status == SurfaceWorkflowAgentStatus::Failed)
            .count() as u32,
        completed_phases: workflow
            .phases
            .iter()
            .filter(|phase| phase.status == SurfaceWorkflowStatus::Completed)
            .count(),
        running_phases: workflow
            .phases
            .iter()
            .filter(|phase| phase.status == SurfaceWorkflowStatus::Running)
            .count(),
        failed_phases: workflow
            .phases
            .iter()
            .filter(|phase| phase.status == SurfaceWorkflowStatus::Failed)
            .count(),
    }
}

fn workflow_terminal_notification(workflow: &SurfaceWorkflow) -> Option<TuiEvent> {
    let status = match workflow.status {
        SurfaceWorkflowStatus::Completed => "completed",
        SurfaceWorkflowStatus::Failed => "failed",
        SurfaceWorkflowStatus::Stopped => "stopped",
        SurfaceWorkflowStatus::Cancelled => "cancelled",
        _ => return None,
    };
    let summary = workflow
        .result
        .as_ref()
        .map(|result| result.content.as_str())
        .or_else(|| workflow.error.as_ref().map(|error| error.as_str()))
        .unwrap_or(status);
    let tool_use_id = workflow
        .result
        .as_ref()
        .and_then(|result| result.tool_use_id.as_ref())
        .map(|tool_use_id| tool_use_id.as_str())
        .unwrap_or("");
    let id = format!(
        "{}:{}:{}",
        workflow.workflow_run_id.as_str(),
        workflow.task_id.as_str(),
        tool_use_id
    );
    let prompt = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<tool-use-id>{}</tool-use-id>\n<run-id>{}</run-id>\n<status>{}</status>\n<summary>{}</summary>\n</task-notification>\n\nA background workflow finished. Use this result to continue the current task.",
        xml_escape(workflow.task_id.as_str()),
        xml_escape(tool_use_id),
        xml_escape(workflow.workflow_run_id.as_str()),
        status,
        xml_escape(summary),
    );
    Some(TuiEvent::WorkflowNotification {
        id,
        prompt,
        status: status.to_string(),
        summary: format!("{}: {summary}", workflow.name.as_str()),
    })
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn task_type(task_type: orca_runtime::surface::SurfaceTaskType) -> TaskType {
    match task_type {
        orca_runtime::surface::SurfaceTaskType::MainSession => TaskType::MainSession,
        orca_runtime::surface::SurfaceTaskType::Workflow => TaskType::Workflow,
        orca_runtime::surface::SurfaceTaskType::Subagent => TaskType::Subagent,
        orca_runtime::surface::SurfaceTaskType::Shell => TaskType::Shell,
        orca_runtime::surface::SurfaceTaskType::Monitor => TaskType::Monitor,
    }
}

fn task_status(status: SurfaceTaskStatus) -> TaskStatus {
    match status {
        SurfaceTaskStatus::Queued => TaskStatus::Queued,
        SurfaceTaskStatus::Running => TaskStatus::Running,
        SurfaceTaskStatus::Paused => TaskStatus::Paused,
        SurfaceTaskStatus::Stopping => TaskStatus::Stopping,
        SurfaceTaskStatus::Stopped => TaskStatus::Stopped,
        SurfaceTaskStatus::Completed => TaskStatus::Completed,
        SurfaceTaskStatus::Failed => TaskStatus::Failed,
        SurfaceTaskStatus::ApprovalRequired => TaskStatus::ApprovalRequired,
        SurfaceTaskStatus::Cancelled => TaskStatus::Cancelled,
    }
}

fn tool_action(action: orca_runtime::surface::SurfaceToolAction) -> ActionKind {
    match action {
        orca_runtime::surface::SurfaceToolAction::Read => ActionKind::Read,
        orca_runtime::surface::SurfaceToolAction::Write => ActionKind::Write,
        orca_runtime::surface::SurfaceToolAction::Network => ActionKind::Network,
        orca_runtime::surface::SurfaceToolAction::Agent => ActionKind::Agent,
        orca_runtime::surface::SurfaceToolAction::Shell => ActionKind::Shell,
    }
}

fn pending_tool_call_for_task(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
    task: &orca_runtime::surface::SurfaceTask,
) -> Option<PendingToolCallSummary> {
    if let Some(tool) = task
        .pending_interaction_id
        .as_ref()
        .and_then(|interaction_id| {
            snapshot
                .interactions
                .iter()
                .find(|interaction| &interaction.interaction_id == interaction_id)
                .and_then(|interaction| match &interaction.request {
                    orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval {
                        tool,
                        ..
                    } => Some(tool),
                    _ => None,
                })
        })
    {
        return Some(pending_tool_call(tool));
    }
    if task.status != SurfaceTaskStatus::ApprovalRequired {
        return None;
    }
    let operation = task.parent_operation.as_ref().and_then(|operation_id| {
        snapshot
            .operation_history
            .iter()
            .chain(snapshot.foreground_operation.iter())
            .chain(snapshot.queued_operations.iter())
            .find(|operation| &operation.operation_id == operation_id)
    })?;
    snapshot
        .tools
        .iter()
        .find(|tool| {
            tool.result.is_none()
                && operation
                    .agent_loop_turns
                    .iter()
                    .any(|turn| turn.turn_id == tool.request.turn_id)
        })
        .map(|tool| pending_tool_call(&tool.request))
}

fn pending_tool_call(tool: &orca_runtime::surface::SurfaceToolRequest) -> PendingToolCallSummary {
    PendingToolCallSummary {
        id: tool.tool_call_id.as_str().to_string(),
        name: tool.name.as_str().to_string(),
        action: tool_action(tool.action),
        target: tool
            .target
            .as_ref()
            .map(|target| target.as_str().to_string()),
        arguments: tool.raw_arguments.as_str().to_string(),
    }
}

fn workflow_status(status: SurfaceWorkflowStatus) -> WorkflowRunStatus {
    match status {
        SurfaceWorkflowStatus::Queued => WorkflowRunStatus::Queued,
        SurfaceWorkflowStatus::Running => WorkflowRunStatus::Running,
        SurfaceWorkflowStatus::Paused => WorkflowRunStatus::Paused,
        SurfaceWorkflowStatus::Stopping => WorkflowRunStatus::Stopping,
        SurfaceWorkflowStatus::Stopped => WorkflowRunStatus::Stopped,
        SurfaceWorkflowStatus::Completed => WorkflowRunStatus::Completed,
        SurfaceWorkflowStatus::Failed => WorkflowRunStatus::Failed,
        SurfaceWorkflowStatus::Cancelled => WorkflowRunStatus::Cancelled,
        SurfaceWorkflowStatus::AsyncLaunched => WorkflowRunStatus::AsyncLaunched,
    }
}

fn workflow_agent_status(status: SurfaceWorkflowAgentStatus) -> WorkflowAgentStatus {
    match status {
        SurfaceWorkflowAgentStatus::Pending => WorkflowAgentStatus::Pending,
        SurfaceWorkflowAgentStatus::Running => WorkflowAgentStatus::Running,
        SurfaceWorkflowAgentStatus::Cached => WorkflowAgentStatus::Cached,
        SurfaceWorkflowAgentStatus::Completed => WorkflowAgentStatus::Completed,
        SurfaceWorkflowAgentStatus::Failed => WorkflowAgentStatus::Failed,
        SurfaceWorkflowAgentStatus::Cancelled => WorkflowAgentStatus::Cancelled,
    }
}

fn core_usage_totals(usage: &orca_runtime::surface::UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_tokens: usage.cache_tokens,
        estimated_cost_usd: usage.estimated_cost_usd_micros as f64 / 1_000_000.0,
    }
}

pub(crate) fn thread_goal_from_surface(
    goal: &SurfaceGoal,
    created_at: UnixMillis,
    updated_at: UnixMillis,
) -> orca_core::goal_types::ThreadGoal {
    let status = match &goal.state {
        SurfaceGoalState::Active => orca_core::goal_types::ThreadGoalStatus::Active,
        SurfaceGoalState::Paused {
            reason: SurfaceGoalPauseReason::NoProgress,
            ..
        } => orca_core::goal_types::ThreadGoalStatus::Stalled,
        SurfaceGoalState::Paused {
            reason: SurfaceGoalPauseReason::UsageLimit,
            ..
        } => orca_core::goal_types::ThreadGoalStatus::UsageLimited,
        SurfaceGoalState::Paused { .. } => orca_core::goal_types::ThreadGoalStatus::Paused,
        SurfaceGoalState::Blocked { .. } => orca_core::goal_types::ThreadGoalStatus::Blocked,
        SurfaceGoalState::BudgetLimited => orca_core::goal_types::ThreadGoalStatus::BudgetLimited,
        SurfaceGoalState::Complete { .. } => orca_core::goal_types::ThreadGoalStatus::Complete,
    };
    orca_core::goal_types::ThreadGoal {
        session_id: surface_thread_id_text(&goal.thread_id),
        objective: goal.objective.as_str().to_string(),
        status,
        token_budget: goal.token_budget,
        tokens_used: goal
            .usage
            .charged_input_tokens
            .saturating_add(goal.usage.output_tokens)
            .saturating_add(goal.usage.verifier_tokens),
        time_used_seconds: goal.usage.elapsed_seconds,
        created_at: created_at.get(),
        updated_at: updated_at.get(),
    }
}

fn surface_thread_id_text(thread_id: &orca_runtime::surface::SurfaceThreadId) -> String {
    let bytes = thread_id.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

fn tool_result_status(kind: SurfaceToolResultKind) -> &'static str {
    match kind {
        SurfaceToolResultKind::Success => "completed",
        SurfaceToolResultKind::Denied => "denied",
        SurfaceToolResultKind::Cancelled => "cancelled",
        SurfaceToolResultKind::TimedOut | SurfaceToolResultKind::InvalidArguments => "failed",
        SurfaceToolResultKind::ExternalEffectAmbiguous
        | SurfaceToolResultKind::ObservationUnavailable
        | SurfaceToolResultKind::CleanupAmbiguous => "indeterminate",
        SurfaceToolResultKind::Failed => "failed",
    }
}

fn operation_terminal_status(terminal: &OperationTerminal) -> Option<&'static str> {
    match terminal {
        OperationTerminal::Succeeded { .. } => Some("success"),
        OperationTerminal::Cancelled { .. } => Some("cancelled"),
        OperationTerminal::BudgetExhausted { .. } => Some("budget_exhausted"),
        OperationTerminal::NotAdmitted { .. } => Some("not_admitted"),
        OperationTerminal::Failed { class, .. } => match class {
            orca_runtime::surface::FailureClass::Verification => Some("verification_failed"),
            _ => Some("failed"),
        },
        OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => Some("failed"),
        OperationTerminal::Shutdown { .. } => Some("cancelled"),
    }
}

fn response_completed_event(response: &SurfaceCompletedModelResponse) -> TuiEvent {
    TuiEvent::AssistantResponseCompleted(
        response
            .message_item
            .as_ref()
            .map(|item| item.text.as_str().to_string()),
        response
            .reasoning_item
            .as_ref()
            .map(|item| item.content.as_str().to_string()),
    )
}

fn response_matches_streamed_items<'a>(
    response: &SurfaceCompletedModelResponse,
    streams: impl Iterator<Item = &'a SurfaceAssistantStream>,
) -> bool {
    let expected = [
        response
            .message_item
            .as_ref()
            .map(|item| (&item.id, AssistantChannel::Message, item.text.as_str())),
        response
            .reasoning_item
            .as_ref()
            .map(|item| (&item.id, AssistantChannel::Reasoning, item.content.as_str())),
        response
            .plan_item
            .as_ref()
            .map(|item| (&item.id, AssistantChannel::Plan, item.text.as_str())),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    // Only streams that back this response's own items count. A logical turn can
    // span several provider responses (tool rounds plus the final answer) that
    // share one turn_id, so matching by turn_id alone would mix in streams from
    // earlier responses and misjudge a streamed response as unstreamed.
    let streams = streams
        .filter(|stream| {
            stream.turn_id == response.turn_id
                && stream.state != SurfaceAssistantStreamState::Discarded
                && expected.iter().any(|(item_id, channel, _)| {
                    &stream.item_id == *item_id && stream.channel == *channel
                })
        })
        .collect::<Vec<_>>();

    streams.len() == expected.len()
        && expected.iter().all(|(item_id, channel, text)| {
            streams.iter().any(|stream| {
                &stream.item_id == *item_id
                    && stream.channel == *channel
                    && stream.text.as_str() == *text
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_runtime::surface::{
        ByteOffset, CanonicalPath, CommitClass, CompactionState, ContextRevision,
        CursorSourceRevision, DisplayText, DurableRevision, GoalCatalogRevision, GoalOwnerEpoch,
        GoalPatch, GoalPatchEnvelope, GoalRevision, GoalUsage, McpCatalogRevision, NonEmptyText,
        NonEmptyVec, PinnedContextRevision, PlanRevision, PolicyEpoch, ProviderReplayHealth,
        SequenceNumber, SessionHealthRevision, SessionMetadataRevision, SettingsRevision,
        Sha256Digest, SurfaceApprovalMode, SurfaceAssistantMessageItem,
        SurfaceAssistantReasoningItem, SurfaceCommitId, SurfaceContextSnapshot,
        SurfaceEventEnvelope, SurfaceEventId, SurfaceGoalId, SurfaceGoalStoreReceipt,
        SurfaceIncarnation, SurfaceInputCorrelationId, SurfaceItemId, SurfaceMcpCatalogSnapshot,
        SurfaceNetworkPermissions, SurfacePermissionRuleSet, SurfacePinnedContextSnapshot,
        SurfacePlanSnapshot, SurfaceReasoningEffort, SurfaceRuntimeSettings, SurfaceScope,
        SurfaceSessionHealth, SurfaceSettingsSnapshot, SurfaceSnapshot, SurfaceTask, SurfaceTaskId,
        SurfaceTaskType, SurfaceThreadId, SurfaceThreadSnapshot, SurfaceTurnId, TaskPatch,
        TaskRevision, ThreadOwnerEpoch, ThreadPersistence, UsageTotals as SurfaceUsageTotals,
        UuidV7, canonical_batch_digest,
    };
    use orca_runtime::surface::{SurfaceGenerationId, SurfaceUsageSnapshot, UsageRevision};

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        bytes
    }

    fn cursor(next_seq: u64, revision: u64) -> SurfaceCursor {
        SurfaceCursor {
            thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(2)).unwrap(),
            next_seq: SequenceNumber::new(next_seq),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(revision).unwrap(),
            },
        }
    }

    fn operation_fence(seed: u8) -> SurfaceOperationFence {
        SurfaceOperationFence {
            thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            generation_id: SurfaceGenerationId::new(u64::from(seed)),
        }
    }

    /// Wrap a completed response as a `ResponseCompleted` envelope with a
    /// deterministic identity derived from `seed`.
    fn response_completed_event_envelope(
        fence: &SurfaceOperationFence,
        turn_id: &SurfaceTurnId,
        reasoning: Option<(SurfaceItemId, &str)>,
        message: Option<(SurfaceItemId, &str)>,
        seed: u8,
    ) -> SurfaceEventEnvelope {
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed + 2)).unwrap(),
        };
        SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
            commit_class,
            scope: SurfaceScope::Generation {
                fence: fence.clone(),
            },
            event: SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                response: SurfaceCompletedModelResponse {
                    response_id: UuidV7::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap(),
                    turn_id: turn_id.clone(),
                    message_item: message.map(|(id, text)| SurfaceAssistantMessageItem {
                        id,
                        turn_id: turn_id.clone(),
                        text: DisplayText::new(text),
                        pinned: false,
                    }),
                    reasoning_item: reasoning.map(|(id, text)| SurfaceAssistantReasoningItem {
                        id,
                        turn_id: turn_id.clone(),
                        summary: DisplayText::new(""),
                        content: DisplayText::new(text),
                        pinned: false,
                    }),
                    plan_item: None,
                    tool_calls: Vec::new(),
                },
            }),
        }
    }

    /// Build a commit batch carrying `events` (sharing the first event's commit
    /// class) and advancing the cursor from `before` to `after`.
    fn commit_batch_with_events(
        before: SurfaceCursor,
        after: SurfaceCursor,
        events: Vec<SurfaceEventEnvelope>,
        digest_seed: u8,
    ) -> SurfaceCommitBatch {
        let event_count = events.len();
        let commit_class = events
            .first()
            .expect("commit batch needs at least one event")
            .commit_class
            .clone();
        SurfaceCommitBatch {
            cursor_before: before,
            cursor_after: after,
            commit_class,
            event_count: u32::try_from(event_count).unwrap(),
            batch_digest: Sha256Digest::new([digest_seed; 32]),
            events: NonEmptyVec::try_new(events).unwrap(),
        }
    }

    fn goal_projection_snapshot() -> SurfaceSnapshot {
        let path =
            CanonicalPath::try_new(std::env::temp_dir().join("orca-tui-goal-projection-owner"))
                .expect("canonical test path");
        SurfaceSnapshot {
            cursor: cursor(0, 1),
            thread: SurfaceThreadSnapshot {
                thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
                owner_epoch: ThreadOwnerEpoch::new(1),
                persistence: ThreadPersistence::RecordedCatalogued,
                title: DisplayText::new("Goal projection test"),
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
                thread_total: SurfaceUsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
                active_operation: None,
                goal: None,
                workflow: Vec::new(),
            },
            context: SurfaceContextSnapshot {
                revision: ContextRevision::try_new(1).unwrap(),
                window_id: orca_runtime::surface::ContextWindowId::new(),
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
                        digest: Sha256Digest::new([1; 32]),
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

    fn goal_projection_goal() -> SurfaceGoal {
        SurfaceGoal {
            goal_id: SurfaceGoalId::try_new("tui-goal-projection").unwrap(),
            thread_id: SurfaceThreadId::try_from_bytes(uuid_v7_bytes(1)).unwrap(),
            goal_revision: GoalRevision::try_new(1).unwrap(),
            goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
            catalog_revision: GoalCatalogRevision::try_new(1).unwrap(),
            receipt_digest: Sha256Digest::new([2; 32]),
            objective: NonEmptyText::try_new("project the committed Goal").unwrap(),
            objective_revision: orca_runtime::surface::GoalObjectiveRevision::new(1),
            state: SurfaceGoalState::Active,
            token_budget: Some(10_000),
            usage: GoalUsage {
                charged_input_tokens: 0,
                output_tokens: 0,
                cache_tokens: 0,
                verifier_tokens: 0,
                cost_micros: 0,
                elapsed_seconds: 0,
            },
            current_run: None,
            last_transition: None,
        }
    }

    fn goal_projection_batch(
        projection: &TuiSurfaceProjection,
        seed: u8,
        receipt: SurfaceGoalStoreReceipt,
        patch: GoalPatch,
    ) -> SurfaceCommitBatch {
        let before = projection.cursor().clone();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(u64::from(seed)).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed.wrapping_add(1))).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Goal {
                goal_id: receipt.goal_id.clone(),
                causative_generation: None,
            },
            event: SurfaceEvent::Goal(GoalPatchEnvelope { receipt, patch }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(before.next_seq.get() + 1),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(u64::from(seed)).unwrap(),
                },
                ..before
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        batch.batch_digest = canonical_batch_digest(&batch);
        batch
    }

    fn task_projection_batch(
        projection: &TuiSurfaceProjection,
        seed: u8,
        task: SurfaceTask,
    ) -> SurfaceCommitBatch {
        let before = projection.cursor().clone();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(u64::from(seed)).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed.wrapping_add(1))).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Task(TaskPatch::Upserted {
                expected_revision: None,
                task,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(before.next_seq.get() + 1),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(u64::from(seed)).unwrap(),
                },
                ..before
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        batch.batch_digest = canonical_batch_digest(&batch);
        batch
    }

    fn assert_single_goal_projection(
        projection: &TuiSurfaceProjection,
        batch: &SurfaceCommitBatch,
        events: &[TuiEvent],
        presentation: GoalProjectionPresentation,
    ) {
        let [TuiEvent::SurfaceProjectionSynced(state)] = events else {
            panic!("Goal batch must end in exactly one projection: {events:?}");
        };
        let reducer_snapshot = projection
            .reducer_state
            .as_ref()
            .expect("Goal projection reducer state")
            .snapshot();
        assert_eq!(state.cursor, batch.cursor_after);
        assert_eq!(
            state.current_goal,
            SurfaceProjectionState::from_surface_snapshot(reducer_snapshot).current_goal
        );
        assert_eq!(state.goal_presentation, Some(presentation));
    }

    #[test]
    fn task_patch_projects_one_authoritative_snapshot() {
        let snapshot = goal_projection_snapshot();
        let mut projection = TuiSurfaceProjection::from_surface_snapshot(&snapshot);
        let task = SurfaceTask {
            task_id: SurfaceTaskId::try_new("task-projection-1").unwrap(),
            revision: TaskRevision::try_new(1).unwrap(),
            task_type: SurfaceTaskType::Workflow,
            status: SurfaceTaskStatus::Running,
            backgrounded: false,
            description: DisplayText::new("project one task snapshot"),
            created_at: UnixMillis::new(1_000),
            started_at: Some(UnixMillis::new(1_000)),
            completed_at: None,
            parent_operation: None,
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
        };
        let batch = task_projection_batch(&projection, 31, task.clone());

        let events = projection.project_typed_batch(&batch).unwrap();

        let [TuiEvent::SurfaceProjectionSynced(state)] = events.as_slice() else {
            panic!("task batch must end in one projection: {events:?}");
        };
        assert_eq!(state.cursor, batch.cursor_after);
        assert!(
            state
                .workflow_tasks
                .iter()
                .any(|item| item.id == task.task_id.as_str())
        );
    }

    #[test]
    fn workflow_task_projection_preserves_parent_identity_and_revision() {
        let mut snapshot = goal_projection_snapshot();
        let parent_id = SurfaceTaskId::try_new("task-parent").unwrap();
        let parent = SurfaceTask {
            task_id: parent_id.clone(),
            revision: TaskRevision::try_new(3).unwrap(),
            task_type: SurfaceTaskType::Workflow,
            status: orca_runtime::surface::SurfaceTaskStatus::Running,
            backgrounded: false,
            description: DisplayText::new("parent task"),
            created_at: UnixMillis::new(1_000),
            started_at: Some(UnixMillis::new(1_000)),
            completed_at: None,
            parent_operation: None,
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
        };
        let child = SurfaceTask {
            task_id: SurfaceTaskId::try_new("task-child").unwrap(),
            revision: TaskRevision::try_new(7).unwrap(),
            task_type: SurfaceTaskType::Subagent,
            status: orca_runtime::surface::SurfaceTaskStatus::Running,
            backgrounded: true,
            description: DisplayText::new("child task"),
            created_at: UnixMillis::new(1_001),
            started_at: Some(UnixMillis::new(1_001)),
            completed_at: None,
            parent_operation: None,
            parent_task_id: Some(parent_id),
            background_fence: None,
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        };
        snapshot.tasks = vec![parent, child];

        let task = workflow_task_summaries(&snapshot)
            .into_iter()
            .find(|task| task.id == "task-child")
            .expect("child task projection");

        assert_eq!(task.parent_task_id.as_deref(), Some("task-parent"));
        assert_eq!(task.publication_revision, Some(7));
    }

    #[test]
    fn surface_session_projection_does_not_invent_ephemeral_session_id() {
        let mut snapshot = goal_projection_snapshot();
        snapshot.thread.persistence = ThreadPersistence::EphemeralNonCataloguedOneShot {
            close_after: orca_runtime::surface::FirstOperationCompletionPolicy::Terminal,
        };
        snapshot.thread.title = DisplayText::new("Ephemeral investigation");
        let projection = SurfaceProjectionState::from_surface_snapshot(&snapshot);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            event_tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );

        state.update(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));

        assert_eq!(
            state.current_session_title(),
            Some("Ephemeral investigation")
        );
        assert!(
            state.current_session_id().is_none(),
            "ephemeral runtime threads are not resumable sessions"
        );
    }

    #[test]
    fn typed_goal_projection_creation_update_and_removal_end_in_one_snapshot() {
        let snapshot = goal_projection_snapshot();
        let mut projection = TuiSurfaceProjection::from_surface_snapshot(&snapshot);
        let goal = goal_projection_goal();
        let created_receipt = SurfaceGoalStoreReceipt {
            goal_id: goal.goal_id.clone(),
            goal_revision: goal.goal_revision,
            objective_revision: goal.objective_revision,
            catalog_revision: goal.catalog_revision,
            goal_owner_epoch: goal.goal_owner_epoch,
            row_state: SurfaceGoalReceiptState::Present {
                state: goal.state.clone(),
                current_run: None,
            },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(20)).unwrap(),
            receipt_digest: Sha256Digest::new([20; 32]),
        };
        let created_batch = goal_projection_batch(
            &projection,
            21,
            created_receipt,
            GoalPatch::Created { goal: goal.clone() },
        );
        let created_events = projection
            .project_typed_batch(&created_batch)
            .expect("project Goal creation");
        assert_single_goal_projection(
            &projection,
            &created_batch,
            &created_events,
            GoalProjectionPresentation::Updated,
        );

        let mut edited_goal = goal.clone();
        edited_goal.goal_revision = GoalRevision::try_new(2).unwrap();
        edited_goal.objective = NonEmptyText::try_new("project the edited Goal").unwrap();
        edited_goal.objective_revision = orca_runtime::surface::GoalObjectiveRevision::new(2);
        let edited_receipt = SurfaceGoalStoreReceipt {
            goal_id: edited_goal.goal_id.clone(),
            goal_revision: edited_goal.goal_revision,
            objective_revision: edited_goal.objective_revision,
            catalog_revision: edited_goal.catalog_revision,
            goal_owner_epoch: edited_goal.goal_owner_epoch,
            row_state: SurfaceGoalReceiptState::Present {
                state: edited_goal.state.clone(),
                current_run: None,
            },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(22)).unwrap(),
            receipt_digest: Sha256Digest::new([22; 32]),
        };
        let edited_batch = goal_projection_batch(
            &projection,
            23,
            edited_receipt,
            GoalPatch::Edited {
                goal_id: edited_goal.goal_id.clone(),
                previous_revision: GoalRevision::try_new(1).unwrap(),
                goal: edited_goal.clone(),
            },
        );
        let edited_events = projection
            .project_typed_batch(&edited_batch)
            .expect("project Goal edit");
        assert_single_goal_projection(
            &projection,
            &edited_batch,
            &edited_events,
            GoalProjectionPresentation::Updated,
        );

        let tombstone_revision = GoalRevision::try_new(3).unwrap();
        let removed_receipt = SurfaceGoalStoreReceipt {
            goal_id: edited_goal.goal_id.clone(),
            goal_revision: tombstone_revision,
            objective_revision: edited_goal.objective_revision,
            catalog_revision: GoalCatalogRevision::try_new(2).unwrap(),
            goal_owner_epoch: edited_goal.goal_owner_epoch,
            row_state: SurfaceGoalReceiptState::Removed { tombstone_revision },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(24)).unwrap(),
            receipt_digest: Sha256Digest::new([24; 32]),
        };
        let removed_batch = goal_projection_batch(
            &projection,
            25,
            removed_receipt,
            GoalPatch::Removed {
                goal_id: edited_goal.goal_id,
                previous_revision: GoalRevision::try_new(2).unwrap(),
                tombstone_revision,
            },
        );
        let removed_events = projection
            .project_typed_batch(&removed_batch)
            .expect("project Goal removal");
        assert_single_goal_projection(
            &projection,
            &removed_batch,
            &removed_events,
            GoalProjectionPresentation::Cleared,
        );
        assert!(
            projection
                .reducer_state
                .as_ref()
                .expect("Goal projection reducer state")
                .snapshot()
                .goal
                .is_none()
        );
    }

    #[test]
    fn typed_goal_projection_without_reducer_rejects_before_goal_mutation() {
        let before = cursor(0, 1);
        let mut projection = TuiSurfaceProjection::from_snapshot(before.clone(), &[]);
        let goal = goal_projection_goal();
        let receipt = SurfaceGoalStoreReceipt {
            goal_id: goal.goal_id.clone(),
            goal_revision: goal.goal_revision,
            objective_revision: goal.objective_revision,
            catalog_revision: goal.catalog_revision,
            goal_owner_epoch: goal.goal_owner_epoch,
            row_state: SurfaceGoalReceiptState::Present {
                state: goal.state.clone(),
                current_run: None,
            },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(30)).unwrap(),
            receipt_digest: Sha256Digest::new([30; 32]),
        };
        let batch = goal_projection_batch(&projection, 31, receipt, GoalPatch::Created { goal });

        assert!(matches!(
            projection.project_typed_batch(&batch),
            Err(SurfaceProjectionError::MissingReducerSnapshot)
        ));
        assert_eq!(projection.cursor(), &before);
        assert!(projection.goal.is_none());
    }

    #[test]
    fn typed_history_projection_preserves_visible_items_and_redacts_secrets() {
        let turn_id = SurfaceTurnId::new();
        let items = vec![
            SurfaceItem::UserMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                input: SurfaceUserInputState::Pending {
                    presentation: SurfaceInputPresentation::Visible {
                        text: DisplayText::new("visible prompt"),
                    },
                    correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(8))
                        .unwrap(),
                },
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::UserInput,
            },
            SurfaceItem::UserMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                input: SurfaceUserInputState::Pending {
                    presentation: SurfaceInputPresentation::Redacted,
                    correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(9))
                        .unwrap(),
                },
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::UserInput,
            },
            SurfaceItem::SystemMessage {
                id: SurfaceItemId::new(),
                content: DisplayText::new("system"),
                pinned: false,
                origin: orca_runtime::surface::SurfaceItemOrigin::RuntimeContext,
            },
            SurfaceItem::AssistantMessage {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                text: DisplayText::new("answer"),
                pinned: false,
            },
            SurfaceItem::AssistantReasoning {
                id: SurfaceItemId::new(),
                turn_id: turn_id.clone(),
                summary: DisplayText::new("summary"),
                content: DisplayText::new("reasoning"),
                pinned: false,
            },
            SurfaceItem::AssistantPlan {
                id: SurfaceItemId::new(),
                turn_id,
                text: DisplayText::new("plan"),
                pinned: false,
            },
        ];

        let messages = history_messages_from_surface_items(&items);
        assert!(matches!(
            messages.as_slice(),
            [
                crate::transcript_state::ChatMessage::User(prompt),
                crate::transcript_state::ChatMessage::System(system),
                crate::transcript_state::ChatMessage::Reasoning(reasoning),
                crate::transcript_state::ChatMessage::Assistant(answer),
                crate::transcript_state::ChatMessage::ProposedPlan(plan),
            ] if prompt == "visible prompt"
                && system == "system"
                && answer == "answer"
                && reasoning == "reasoning"
                && plan == "plan"
        ));
    }

    #[test]
    fn typed_assistant_delta_projects_only_after_stream_identity_is_known() {
        let before = cursor(0, 1);
        let stream_id = SurfaceStreamId::try_from_bytes(uuid_v7_bytes(3)).unwrap();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: orca_runtime::surface::ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
        };
        let after = SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(2).unwrap(),
            },
            ..before.clone()
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(5)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Assistant(AssistantPatch::Delta {
                stream_id: stream_id.clone(),
                offset: ByteOffset::new(0),
                text: DisplayText::new("hello"),
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: after,
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        let mut projection = TuiSurfaceProjection::from_snapshot(before, &[]);

        assert!(matches!(
            projection.reduce_typed_batch(&batch),
            Err(SurfaceProjectionError::UnknownAssistantStream { stream_id: observed })
                if observed == stream_id
        ));
    }

    #[test]
    fn completed_response_does_not_reproject_identical_streamed_content() {
        let before = cursor(0, 1);
        let fence = operation_fence(12);
        let turn_id = SurfaceTurnId::new();
        let reasoning_item_id = SurfaceItemId::new();
        let message_item_id = SurfaceItemId::new();
        let reasoning_stream = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(13))
                .expect("reasoning stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: reasoning_item_id.clone(),
            channel: AssistantChannel::Reasoning,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("reason"),
            state: SurfaceAssistantStreamState::Open,
        };
        let message_stream = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(14))
                .expect("message stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: message_item_id.clone(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("answer"),
            state: SurfaceAssistantStreamState::Open,
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(15)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(16)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Generation { fence },
            event: SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                response: SurfaceCompletedModelResponse {
                    response_id: UuidV7::try_from_bytes(uuid_v7_bytes(17)).unwrap(),
                    turn_id: turn_id.clone(),
                    message_item: Some(SurfaceAssistantMessageItem {
                        id: message_item_id,
                        turn_id: turn_id.clone(),
                        text: DisplayText::new("answer"),
                        pinned: false,
                    }),
                    reasoning_item: Some(SurfaceAssistantReasoningItem {
                        id: reasoning_item_id,
                        turn_id,
                        summary: DisplayText::new(""),
                        content: DisplayText::new("reason"),
                        pinned: false,
                    }),
                    plan_item: None,
                    tool_calls: Vec::new(),
                },
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(1),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(2).unwrap(),
                },
                ..before.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        let mut projection =
            TuiSurfaceProjection::from_snapshot(before, &[reasoning_stream, message_stream]);

        assert!(projection.reduce_typed_batch(&batch).unwrap().is_empty());
        assert!(
            projection
                .assistant_streams
                .values()
                .all(|stream| stream.state == SurfaceAssistantStreamState::Completed)
        );
    }

    #[test]
    fn completed_response_ignores_streams_from_earlier_responses_of_same_turn() {
        let fence = operation_fence(22);
        let turn_id = SurfaceTurnId::new();
        let earlier_reasoning_id = SurfaceItemId::new();
        let earlier_message_id = SurfaceItemId::new();
        let final_message_id = SurfaceItemId::new();
        let earlier_reasoning = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(23))
                .expect("earlier reasoning stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: earlier_reasoning_id.clone(),
            channel: AssistantChannel::Reasoning,
            next_offset: ByteOffset::new(7),
            text: DisplayText::new("tool round thinking"),
            state: SurfaceAssistantStreamState::Open,
        };
        let earlier_message = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(24))
                .expect("earlier message stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: earlier_message_id.clone(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("checking docs"),
            state: SurfaceAssistantStreamState::Open,
        };
        let mut projection = TuiSurfaceProjection::from_snapshot(
            cursor(0, 1),
            &[earlier_reasoning, earlier_message],
        );

        // Response 1 (a tool round) completes: its streams are marked Completed
        // and remain in the projection; nothing is reprojected because the
        // response was streamed.
        let first_response = response_completed_event_envelope(
            &fence,
            &turn_id,
            Some((earlier_reasoning_id, "tool round thinking")),
            Some((earlier_message_id, "checking docs")),
            26,
        );
        let first_batch =
            commit_batch_with_events(cursor(0, 1), cursor(1, 2), vec![first_response], 2);
        assert!(
            projection
                .reduce_typed_batch(&first_batch)
                .unwrap()
                .is_empty()
        );
        assert!(
            projection
                .assistant_streams
                .values()
                .all(|stream| stream.state == SurfaceAssistantStreamState::Completed)
        );

        // Response 2 (the final answer) streams a message item in the same turn
        // and then completes. Its own stream backs the item, so the full-response
        // event must not fire even though the earlier streams are still present.
        let final_message = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(25))
                .expect("final message stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: final_message_id.clone(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(12),
            text: DisplayText::new("final answer"),
            state: SurfaceAssistantStreamState::Open,
        };
        let opened = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(29)).unwrap(),
            commit_class: CommitClass::Recorded {
                thread_owner_epoch: ThreadOwnerEpoch::new(1),
                durable_revision: DurableRevision::try_new(2).unwrap(),
                commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(30)).unwrap(),
            },
            scope: SurfaceScope::Generation {
                fence: fence.clone(),
            },
            event: SurfaceEvent::Assistant(AssistantPatch::StreamOpened {
                stream: final_message,
            }),
        };
        let second_response = response_completed_event_envelope(
            &fence,
            &turn_id,
            None,
            Some((final_message_id, "final answer")),
            31,
        );
        let second_batch =
            commit_batch_with_events(cursor(1, 2), cursor(2, 3), vec![opened, second_response], 3);
        assert!(
            projection
                .reduce_typed_batch(&second_batch)
                .unwrap()
                .is_empty()
        );
        assert!(
            projection
                .assistant_streams
                .values()
                .all(|stream| stream.state == SurfaceAssistantStreamState::Completed)
        );
    }

    #[test]
    fn unstreamed_completed_response_still_emits_full_response_event() {
        let fence = operation_fence(42);
        let turn_id = SurfaceTurnId::new();
        let message_id = SurfaceItemId::new();
        let mut projection = TuiSurfaceProjection::from_snapshot(cursor(0, 1), &[]);

        // No stream ever backed this response's item (e.g. a resumed or cached
        // response with no deltas), so the full-response event must fire for the
        // TUI to render the completed content.
        let event = response_completed_event_envelope(
            &fence,
            &turn_id,
            None,
            Some((message_id, "completed answer")),
            43,
        );
        let batch = commit_batch_with_events(cursor(0, 1), cursor(1, 2), vec![event], 2);
        assert!(matches!(
            projection.reduce_typed_batch(&batch).unwrap().as_slice(),
            [TuiEvent::AssistantResponseCompleted(Some(message), None)]
                if message == "completed answer"
        ));
    }

    #[test]
    fn completed_response_without_items_ignores_earlier_streams_of_same_turn() {
        let fence = operation_fence(32);
        let turn_id = SurfaceTurnId::new();
        let earlier_streams = [
            SurfaceAssistantStream {
                stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(33))
                    .expect("earlier reasoning stream id"),
                fence: fence.clone(),
                turn_id: turn_id.clone(),
                item_id: SurfaceItemId::new(),
                channel: AssistantChannel::Reasoning,
                next_offset: ByteOffset::new(7),
                text: DisplayText::new("tool round thinking"),
                state: SurfaceAssistantStreamState::Completed,
            },
            SurfaceAssistantStream {
                stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(34))
                    .expect("earlier message stream id"),
                fence: fence.clone(),
                turn_id: turn_id.clone(),
                item_id: SurfaceItemId::new(),
                channel: AssistantChannel::Message,
                next_offset: ByteOffset::new(6),
                text: DisplayText::new("checking docs"),
                state: SurfaceAssistantStreamState::Completed,
            },
        ];
        let event = response_completed_event_envelope(&fence, &turn_id, None, None, 36);
        let batch = commit_batch_with_events(cursor(0, 1), cursor(1, 2), vec![event], 3);
        let mut projection = TuiSurfaceProjection::from_snapshot(cursor(0, 1), &earlier_streams);

        // A response with no items has nothing to reproject, so the full-response
        // event must not fire merely because unrelated same-turn streams exist.
        assert!(projection.reduce_typed_batch(&batch).unwrap().is_empty());
    }

    #[test]
    fn typed_usage_projection_waits_for_commit_snapshot() {
        let before = cursor(0, 1);
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(5)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Usage(SurfaceUsageSnapshot {
                revision: UsageRevision::try_new(17).unwrap(),
                thread_total: orca_runtime::surface::UsageTotals {
                    input_tokens: 8_000,
                    output_tokens: 900,
                    cache_tokens: 450,
                    estimated_cost_usd_micros: 35_000,
                },
                active_operation: None,
                goal: None,
                workflow: Vec::new(),
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: before.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(1),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(2).unwrap(),
                },
                ..before.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        let mut projection = TuiSurfaceProjection::from_snapshot(before.clone(), &[]);

        assert!(
            projection
                .reduce_typed_batch(&batch)
                .expect("valid usage batch")
                .is_empty()
        );

        let mut projection = TuiSurfaceProjection::from_snapshot(before.clone(), &[]);
        assert!(matches!(
            projection.project_typed_batch(&batch),
            Err(SurfaceProjectionError::MissingReducerSnapshot)
        ));
        assert_eq!(projection.cursor(), &before);
    }

    #[test]
    fn typed_context_projection_waits_for_commit_snapshot() {
        let before = cursor(0, 1);
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(18)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(19)).unwrap(),
            commit_class,
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Context(orca_runtime::surface::SurfaceContextSnapshot {
                revision: orca_runtime::surface::ContextRevision::try_new(2).unwrap(),
                window_id: orca_runtime::surface::ContextWindowId::new(),
                used_tokens: 4_096,
                limit_tokens: 128_000,
                compaction: orca_runtime::surface::CompactionState::Idle,
                fragments: Vec::new(),
                provider_replay: orca_runtime::surface::ProviderReplayHealth::None,
            }),
        };
        let after = SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(2).unwrap(),
            },
            ..before.clone()
        };
        let batch = commit_batch_with_events(before.clone(), after, vec![event], 20);
        let mut projection = TuiSurfaceProjection::from_snapshot(before, &[]);

        assert!(
            projection
                .reduce_typed_batch(&batch)
                .expect("valid context batch")
                .is_empty()
        );
    }

    #[test]
    fn cursor_gap_is_rejected_without_advancing_projection() {
        let expected = cursor(3, 2);
        let observed = cursor(4, 3);
        let mut projection = TuiSurfaceProjection::from_snapshot(expected.clone(), &[]);
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: orca_runtime::surface::ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(4).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(6)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(7)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Context(orca_runtime::surface::SurfaceContextSnapshot {
                revision: orca_runtime::surface::ContextRevision::try_new(2).unwrap(),
                window_id: orca_runtime::surface::ContextWindowId::new(),
                used_tokens: 1,
                limit_tokens: 2,
                compaction: orca_runtime::surface::CompactionState::Idle,
                fragments: Vec::new(),
                provider_replay: orca_runtime::surface::ProviderReplayHealth::None,
            }),
        };
        let batch = SurfaceCommitBatch {
            cursor_before: observed.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(5),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(4).unwrap(),
                },
                ..observed.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([1; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };

        assert!(matches!(
            projection.reduce_typed_batch(&batch),
            Err(SurfaceProjectionError::CursorGap {
                expected: gap_expected,
                observed: gap_observed,
            }) if gap_expected == expected && gap_observed == observed
        ));
        assert_eq!(projection.cursor(), &expected);
    }

    #[test]
    fn typed_tool_statuses_use_existing_tui_vocabulary() {
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::Success),
            "completed"
        );
        assert_eq!(tool_result_status(SurfaceToolResultKind::Denied), "denied");
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::Cancelled),
            "cancelled"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::TimedOut),
            "failed"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::InvalidArguments),
            "failed"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::ExternalEffectAmbiguous),
            "indeterminate"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::ObservationUnavailable),
            "indeterminate"
        );
        assert_eq!(
            tool_result_status(SurfaceToolResultKind::CleanupAmbiguous),
            "indeterminate"
        );
        assert_eq!(tool_result_status(SurfaceToolResultKind::Failed), "failed");
    }

    #[test]
    fn snapshot_hydration_emits_running_turn_once() {
        let mut projection = TuiSurfaceProjection {
            cursor: cursor(0, 1),
            assistant_streams: BTreeMap::new(),
            assistant_stream_order: Vec::new(),
            completed_items: Vec::new(),
            operation_turn_ids: BTreeMap::new(),
            focused_operation: None,
            pending_turn_started: Some(TuiTaskLifecycle {
                id: "task-1".to_string(),
                kind: "agent".to_string(),
                status: "running".to_string(),
                turn: 4,
            }),
            goal: None,
            reducer_state: None,
        };

        assert!(matches!(
            projection.hydrate_open_streams().as_slice(),
            [TuiEvent::TurnStarted {
                turn: 4,
                task: Some(TuiTaskLifecycle { id, status, .. }),
            }] if id == "task-1" && status == "running"
        ));
        assert!(projection.hydrate_open_streams().is_empty());
    }

    #[test]
    fn snapshot_hydration_preserves_assistant_stream_open_order() {
        let fence = operation_fence(10);
        let turn_id = SurfaceTurnId::new();
        let reasoning = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(31))
                .expect("reasoning stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Reasoning,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("reason"),
            state: SurfaceAssistantStreamState::Open,
        };
        let message = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(30))
                .expect("message stream id"),
            fence,
            turn_id,
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("answer"),
            state: SurfaceAssistantStreamState::Open,
        };
        let mut projection =
            TuiSurfaceProjection::from_snapshot(cursor(0, 1), &[reasoning, message]);

        assert!(matches!(
            projection.hydrate_open_streams().as_slice(),
            [TuiEvent::ReasoningDelta(reasoning), TuiEvent::MessageDelta(message)]
                if reasoning == "reason" && message == "answer"
        ));
    }

    #[test]
    fn foreground_hydration_emits_only_undelivered_bytes_for_the_selected_operation() {
        let fence = operation_fence(10);
        let other_fence = operation_fence(20);
        let message_stream_id =
            SurfaceStreamId::try_from_bytes(uuid_v7_bytes(30)).expect("message stream id");
        let reasoning_stream_id =
            SurfaceStreamId::try_from_bytes(uuid_v7_bytes(31)).expect("reasoning stream id");
        let other_stream_id =
            SurfaceStreamId::try_from_bytes(uuid_v7_bytes(32)).expect("other stream id");
        let streams = vec![
            SurfaceAssistantStream {
                stream_id: message_stream_id.clone(),
                fence: fence.clone(),
                turn_id: SurfaceTurnId::new(),
                item_id: SurfaceItemId::new(),
                channel: AssistantChannel::Message,
                next_offset: ByteOffset::new(12),
                text: DisplayText::new("hello world!"),
                state: SurfaceAssistantStreamState::Completed,
            },
            SurfaceAssistantStream {
                stream_id: reasoning_stream_id.clone(),
                fence: fence.clone(),
                turn_id: SurfaceTurnId::new(),
                item_id: SurfaceItemId::new(),
                channel: AssistantChannel::Reasoning,
                next_offset: ByteOffset::new(6),
                text: DisplayText::new("reason"),
                state: SurfaceAssistantStreamState::Open,
            },
            SurfaceAssistantStream {
                stream_id: other_stream_id,
                fence: other_fence,
                turn_id: SurfaceTurnId::new(),
                item_id: SurfaceItemId::new(),
                channel: AssistantChannel::Message,
                next_offset: ByteOffset::new(5),
                text: DisplayText::new("other"),
                state: SurfaceAssistantStreamState::Open,
            },
        ];
        let initial_stream = SurfaceAssistantStream {
            stream_id: message_stream_id.clone(),
            fence: fence.clone(),
            turn_id: streams[0].turn_id.clone(),
            item_id: streams[0].item_id.clone(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(5),
            text: DisplayText::new("hello"),
            state: SurfaceAssistantStreamState::Open,
        };
        let initial = TuiSurfaceProjection::from_snapshot(cursor(0, 1), &[initial_stream]);
        let watermark = initial.delivery_watermark(&fence.operation_id);
        let projection = TuiSurfaceProjection::from_snapshot(cursor(0, 1), &streams);

        let hydrated = projection
            .hydrate_after_delivery_watermark(&fence.operation_id, &watermark)
            .expect("valid delivery watermark");
        assert!(matches!(
            hydrated.as_slice(),
            [TuiEvent::MessageDelta(message), TuiEvent::ReasoningDelta(reasoning)]
                if message == " world!" && reasoning == "reason"
        ));
        assert_eq!(
            projection
                .delivery_watermark(&fence.operation_id)
                .get(&message_stream_id),
            Some(&ByteOffset::new(12))
        );
        assert_eq!(
            projection
                .delivery_watermark(&fence.operation_id)
                .get(&reasoning_stream_id),
            Some(&ByteOffset::new(6))
        );
    }

    #[test]
    fn foreground_hydration_preserves_assistant_stream_open_order() {
        let fence = operation_fence(33);
        let turn_id = SurfaceTurnId::new();
        let reasoning = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(35))
                .expect("reasoning stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Reasoning,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("reason"),
            state: SurfaceAssistantStreamState::Open,
        };
        let message = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(34))
                .expect("message stream id"),
            fence: fence.clone(),
            turn_id,
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(6),
            text: DisplayText::new("answer"),
            state: SurfaceAssistantStreamState::Open,
        };
        let projection = TuiSurfaceProjection::from_snapshot(cursor(0, 1), &[reasoning, message]);

        assert!(matches!(
            projection
                .hydrate_after_delivery_watermark(
                    &fence.operation_id,
                    &TuiStreamDeliveryWatermark::new(),
                )
                .expect("valid delivery watermark")
                .as_slice(),
            [TuiEvent::ReasoningDelta(reasoning), TuiEvent::MessageDelta(message)]
                if reasoning == "reason" && message == "answer"
        ));
    }

    #[test]
    fn foreground_hydration_reconciles_completed_item_for_discarded_stream() {
        let fence = operation_fence(40);
        let turn_id = SurfaceTurnId::new();
        let item_id = SurfaceItemId::new();
        let stream = SurfaceAssistantStream {
            stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(41))
                .expect("discarded stream id"),
            fence: fence.clone(),
            turn_id: turn_id.clone(),
            item_id: item_id.clone(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(7),
            text: DisplayText::new("partial"),
            state: SurfaceAssistantStreamState::Discarded,
        };
        let stream_id = stream.stream_id.clone();
        let projection = TuiSurfaceProjection {
            cursor: cursor(0, 1),
            assistant_streams: BTreeMap::from([(stream_id.clone(), stream)]),
            assistant_stream_order: vec![stream_id],
            completed_items: vec![SurfaceItem::AssistantMessage {
                id: item_id,
                turn_id: turn_id.clone(),
                text: DisplayText::new("corrected full response"),
                pinned: false,
            }],
            operation_turn_ids: BTreeMap::from([(fence.operation_id.clone(), vec![turn_id])]),
            focused_operation: None,
            pending_turn_started: None,
            goal: None,
            reducer_state: None,
        };

        assert!(matches!(
            projection
                .hydrate_after_delivery_watermark(
                    &fence.operation_id,
                    &TuiStreamDeliveryWatermark::new(),
                )
                .expect("discarded stream hydration")
                .as_slice(),
            [TuiEvent::AssistantResponseCompleted(Some(message), None)]
                if message == "corrected full response"
        ));
    }
}
