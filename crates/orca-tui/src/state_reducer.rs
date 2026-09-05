//! Event reduction for the TUI state machine.

use std::time::Instant;

use orca_core::approval_types::ApprovalMode;
use orca_core::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};

use crate::display_text::truncate_to_display_width;
use crate::protocol::{
    PendingTuiInput, PendingWorkflowNotification, TuiEvent, TuiMcpElicitationMode,
};
use crate::streaming_markdown::{StreamingMarkdownAction, StreamingMarkdownAssembler};
use crate::surface_projection::{
    SurfaceGoalProjectionEffect, SurfaceOperationProjectionApply, SurfaceOperationProjectionEffect,
    SurfaceOperationProjectionState, SurfaceProjectionState, SurfaceSessionProjectionApply,
    SurfaceSessionProjectionEffect, SurfaceSessionProjectionState,
};
use crate::transcript_state::ChatMessage;
use crate::types::{
    AppState, AppStatus, ApprovalDialog, PanelMode, PlanApprovalDialog, SessionPickerPhase,
    SideConversationUiState, TaskTranscriptViewState,
};
use crate::user_input_dialog::UserInputDialog;

const GOAL_NOTICE_OBJECTIVE_WIDTH: usize = 80;

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

impl AppState {
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
            TuiEvent::ChildFocusChanged { task_id } => {
                self.set_focused_child_task_id(task_id);
                self.task_transcript = None;
                self.panel_mode = PanelMode::Conversation;
                self.scroll_to_bottom();
            }
            TuiEvent::SurfaceProjectionSynced(projection) => {
                self.apply_surface_projection_state(*projection);
            }
            TuiEvent::NewSessionStarted => {
                self.task_transcript = None;
                self.agent_dock_selected_task_id = None;
                self.set_focused_child_task_id(None);
                self.announced_subagent_batches.clear();
            }
            TuiEvent::SessionProjectionReset(projection) => {
                if !SurfaceSessionProjectionState::accepts_reset(&projection)
                    || !SurfaceOperationProjectionState::accepts_reset(&projection)
                {
                    return;
                }
                self.reset_session_projection();
                self.task_transcript = None;
                self.apply_surface_projection_state(*projection);
            }
            TuiEvent::ChildProjectionReset {
                task_id,
                projection,
            } => {
                if !SurfaceSessionProjectionState::accepts_reset(&projection)
                    || !SurfaceOperationProjectionState::accepts_reset(&projection)
                {
                    return;
                }
                let background_tasks = self.background_workflow_tasks.clone();
                self.reset_session_projection();
                self.background_workflow_tasks = background_tasks;
                self.set_focused_child_task_id(Some(task_id));
                self.task_transcript = None;
                self.panel_mode = PanelMode::Conversation;
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
                self.transcript.finalized_count = self.transcript.messages.len();
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::TurnStarted { .. } => {
                self.suppress_background_main_session_output = false;
                self.enter_running();
            }
            TuiEvent::QueuedSubmissionStarted { id } => {
                let _ = id;
                self.enter_running();
            }
            TuiEvent::PromptQueueUpdated(snapshot) => {
                if snapshot.running_item().is_some() {
                    self.enter_running();
                }
                self.replace_runtime_queue_projection(snapshot);
            }
            TuiEvent::PromptQueueControlUpdated {
                deleted_id,
                snapshot,
            } => {
                if snapshot.running_item().is_some() {
                    self.enter_running();
                }
                self.replace_runtime_queue_control_projection(snapshot, deleted_id.as_ref());
            }
            TuiEvent::BackgroundTaskOutputAttached { .. } => {
                self.suppress_background_main_session_output = false;
                self.panel_mode = PanelMode::Conversation;
            }
            TuiEvent::TaskTranscriptResult { request, result } => {
                if !self
                    .task_transcript
                    .as_ref()
                    .is_some_and(|current| current.request == request)
                {
                    return;
                }
                self.task_transcript = Some(TaskTranscriptViewState {
                    request,
                    result: Some(result),
                    scroll: 0,
                });
            }
            TuiEvent::ReasoningDelta(text) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.finish_assistant_stream();
                let last = self.transcript.messages.len().saturating_sub(1);
                if matches!(
                    self.transcript.messages.last(),
                    Some(ChatMessage::Reasoning(_))
                ) {
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
            TuiEvent::AssistantAttemptDiscarded => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.discard_current_assistant_attempt();
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
                if let Some(index) = self.transcript.messages.iter().rposition(|message| {
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
                let message_index = if let Some(index) =
                    self.transcript.messages.iter().rposition(|message| {
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
                    let index = self.transcript.messages.len();
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
            TuiEvent::WorkflowTasksUpdated(tasks) => {
                if !self.suppress_background_main_session_output {
                    self.apply_workflow_tasks_update(tasks);
                }
            }
            TuiEvent::BackgroundTasksUpdated(tasks) => {
                self.apply_background_tasks_update(tasks);
            }
            TuiEvent::TaskStatusUpdated(task) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                let mut tasks = self.workflow_tasks().to_vec();
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
                self.user_input_dialog = None;
                self.interaction.pending_submission = None;
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
                self.user_input_dialog = None;
                self.interaction.pending_submission = None;
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
                self.interaction.pending_input = Some(PendingTuiInput::UserInput(key));
                self.interaction.pending_mcp_elicitation_mode = None;
                self.interaction.pending_submission = None;
                self.finish_assistant_stream();
                self.slash_menu = None;
                self.mention.clear_projection();
                self.user_input_dialog =
                    (!choices.is_empty()).then(|| UserInputDialog::new(&question, choices));
                self.push_message(ChatMessage::System(question));
            }
            TuiEvent::McpElicitationRequested {
                key,
                server_name,
                mode,
                message,
                url,
                requested_schema_json,
            } => {
                self.user_input_dialog = None;
                self.set_status(AppStatus::WaitingUserInput);
                self.interaction.pending_input = Some(PendingTuiInput::McpElicitation(key));
                self.interaction.pending_mcp_elicitation_mode = Some(mode.clone());
                self.interaction.pending_submission = None;
                self.finish_assistant_stream();
                let mut lines = vec![format!("MCP {server_name} requests input: {message}")];
                match mode {
                    TuiMcpElicitationMode::Form => {
                        lines.push("Mode: form".to_string());
                        if let Some(schema) = requested_schema_json {
                            lines.push(format!("Schema: {schema}"));
                        }
                    }
                    TuiMcpElicitationMode::Url => {
                        lines.push("Mode: url".to_string());
                        if let Some(url) = url {
                            lines.push(format!("URL: {url}"));
                        }
                    }
                }
                self.push_message(ChatMessage::System(lines.join("\n")));
            }
            TuiEvent::SubmissionRejected {
                queued_id: _,
                prompt: _,
                bindings: _,
                images: _,
                message,
            } => {
                self.remove_after_last_user();
                self.mention_bindings.clear();
                self.atomic_skill_tokens.clear();
                self.clear_receiving_tool_progress();
                self.push_message(ChatMessage::Error(message));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::OperationRejected(message) => {
                self.cancel_latest_queued_edit();
                self.user_input_dialog = None;
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
            | TuiEvent::MentionRuntimeReady(_)
            | TuiEvent::ClipboardImagePasteCompleted { .. } => {}
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
                self.interaction.pending_input = None;
                self.interaction.pending_mcp_elicitation_mode = None;
                self.interaction.pending_submission = None;
                self.user_input_dialog = None;
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
                    self.request_runtime_queue_pause();
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

    pub(crate) fn push_goal_notice(&mut self, notice: String) {
        // The live Goal banner is the source of truth for current status, so the
        // transcript only needs a notice when the rendered line actually changes.
        // Collapsing consecutive identical notices keeps the periodic refreshes
        // emitted between auto-continuation turns (which land while the app is
        // Idle) from stacking duplicate lines.
        let duplicate = self
            .transcript
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

    pub(crate) fn receiving_tool_call_message_index(&self, id: &str) -> Option<usize> {
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
        let first_message = self.transcript.messages.get(first)?;
        if is_receiving(first_message) {
            return Some(first);
        }
        self.transcript
            .messages
            .get(first + 1..)?
            .iter()
            .rposition(is_receiving)
            .map(|offset| first + 1 + offset)
    }

    /// Materialize the live surface's child work into the parent transcript.
    /// The task DTO is already derived from the surface ledger, so this keeps
    /// the conversation and agent dock on one identity-bound source.
    pub(crate) fn sync_subagent_transcript_messages(&mut self) {
        let existing_subagent_ids = self
            .transcript
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Subagent { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let tasks = self
            .workflow_tasks()
            .iter()
            .filter(|task| {
                task.task_type == orca_core::task_types::TaskType::Subagent
                    && (task.status.is_active()
                        || task.status.requires_attention()
                        || existing_subagent_ids.contains(&task.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        for task in &tasks {
            let batch_key = task
                .subagent_batch_id
                .clone()
                .unwrap_or_else(|| task.id.clone());
            if (task.status.is_active() || task.status.requires_attention())
                && self.announced_subagent_batches.insert(batch_key)
            {
                self.finish_assistant_stream();
                let batch_size = task.subagent_batch_size.unwrap_or(1).max(1);
                let noun = if batch_size == 1 {
                    "agent"
                } else {
                    "agents in parallel"
                };
                self.push_message(ChatMessage::System(format!(
                    "Delegating to {batch_size} {noun}"
                )));
            }

            let next = ChatMessage::Subagent {
                id: task.id.clone(),
                description: task.description.clone(),
                status: surface_task_status_label(task.status).to_string(),
                output: task.result.clone(),
                error: task.error.clone(),
                activity: task.subagent_current_activity.clone(),
                activity_tail: task
                    .subagent_activity_history
                    .iter()
                    .map(|entry| entry.activity.clone())
                    .collect(),
                turn: task.subagent_turn,
                usage: task.usage,
                expanded: task.status == orca_core::task_types::TaskStatus::Running,
            };
            if let Some(index) = self.transcript.messages.iter().position(
                |message| matches!(message, ChatMessage::Subagent { id, .. } if id == &task.id),
            ) {
                let expanded = match &self.transcript.messages[index] {
                    ChatMessage::Subagent { expanded, .. } => {
                        *expanded || task.status == orca_core::task_types::TaskStatus::Running
                    }
                    _ => false,
                };
                let mut next = next;
                if let ChatMessage::Subagent {
                    expanded: value, ..
                } = &mut next
                {
                    *value = expanded;
                }
                self.replace_message(index, next);
            } else {
                self.finish_assistant_stream();
                self.push_message(next);
            }
        }
    }

    pub(crate) fn apply_surface_projection_state(&mut self, projection: SurfaceProjectionState) {
        let mut projection = projection;
        if self.focused_child_task_id.is_some() {
            projection.workflow_tasks = merge_background_task_snapshots(
                &self.background_workflow_tasks,
                &projection.workflow_tasks,
            );
        }
        let mut surface_session = self.surface_session.clone();
        let session_apply = surface_session.apply_projection(&projection);
        let mut surface_operation = self.surface_operation.clone();
        let operation_apply = surface_operation.apply_projection(&projection);
        let mut surface_workflow_tasks = self.surface_workflow_tasks.clone();
        let workflow_tasks_accepted = surface_workflow_tasks.apply_projection(&projection);
        if matches!(session_apply, SurfaceSessionProjectionApply::Rejected)
            || matches!(operation_apply, SurfaceOperationProjectionApply::Rejected)
            || !workflow_tasks_accepted
            || self
                .surface_metrics
                .rejects_usage_revision(projection.usage_revision)
        {
            return;
        }
        self.surface_session = surface_session;
        self.surface_operation = surface_operation;
        self.surface_workflow_tasks = surface_workflow_tasks;
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

    pub(crate) fn promote_trailing_reasoning(&mut self) {
        let index = self.transcript.messages.len().saturating_sub(1);
        if let Some(ChatMessage::Reasoning(text)) = self.transcript.messages.get(index) {
            let text = text.clone();
            self.replace_message(index, ChatMessage::Assistant(text));
        }
    }

    pub(crate) fn reconcile_assistant_response(
        &mut self,
        message: Option<&str>,
        reasoning: Option<&str>,
    ) {
        let last_user = self
            .transcript
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
        self.transcript.proposed_plan_parser = ProposedPlanStreamParser::default();
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

    pub(crate) fn discard_current_assistant_attempt(&mut self) {
        let boundary = self.transcript.messages.iter().rposition(|message| {
            !matches!(
                message,
                ChatMessage::Reasoning(_)
                    | ChatMessage::Assistant(_)
                    | ChatMessage::AssistantChunk { .. }
                    | ChatMessage::ProposedPlan(_)
            )
        });
        let mut index = 0_usize;
        self.retain_messages(|message| {
            let keep = boundary.is_some_and(|boundary| index <= boundary)
                || !matches!(
                    message,
                    ChatMessage::Reasoning(_)
                        | ChatMessage::Assistant(_)
                        | ChatMessage::AssistantChunk { .. }
                        | ChatMessage::ProposedPlan(_)
                );
            index += 1;
            keep
        });
        self.transcript.proposed_plan_parser = ProposedPlanStreamParser::default();
        self.reset_assistant_stream();
    }

    pub(crate) fn handle_message_delta(&mut self, text: &str) {
        for segment in self.transcript.proposed_plan_parser.push(text) {
            self.push_proposed_plan_segment(segment);
        }
    }

    pub(crate) fn flush_proposed_plan_parser(&mut self) {
        for segment in self.transcript.proposed_plan_parser.finish() {
            self.push_proposed_plan_segment(segment);
        }
    }

    pub(crate) fn push_proposed_plan_segment(&mut self, segment: ProposedPlanSegment) {
        match segment {
            ProposedPlanSegment::Agent(text) => {
                let actions = self.transcript.assistant_stream.push(&text);
                self.apply_streaming_markdown_actions(actions);
            }
            ProposedPlanSegment::Plan(text) => {
                self.finish_assistant_stream();
                self.push_proposed_plan_delta(text);
            }
        }
    }

    pub(crate) fn apply_streaming_markdown_actions(
        &mut self,
        actions: Vec<StreamingMarkdownAction>,
    ) {
        for action in actions {
            match action {
                StreamingMarkdownAction::UpdateTail(text) => {
                    if let Some(index) = self.transcript.assistant_stream_tail {
                        self.mutate_message(index, |message| {
                            let ChatMessage::Assistant(existing) = message else {
                                unreachable!();
                            };
                            *existing = text;
                        });
                    } else {
                        let index = self.transcript.messages.len();
                        self.push_message(ChatMessage::Assistant(text));
                        self.transcript.assistant_stream_tail = Some(index);
                    }
                }
                StreamingMarkdownAction::FreezeTail {
                    text,
                    trailing_blank,
                } => {
                    if let Some(index) = self.transcript.assistant_stream_tail {
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
                    self.transcript.assistant_stream_tail = None;
                }
                StreamingMarkdownAction::FinishTail(suffix) => {
                    if let Some(index) = self.transcript.assistant_stream_tail {
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
                    self.transcript.assistant_stream_tail = None;
                }
            }
        }
    }

    pub(crate) fn finish_assistant_stream(&mut self) {
        let actions = self.transcript.assistant_stream.finish();
        self.apply_streaming_markdown_actions(actions);
        self.transcript.assistant_stream = StreamingMarkdownAssembler::default();
        self.transcript.assistant_stream_tail = None;
        let Some(index) = self.transcript.messages.len().checked_sub(1) else {
            return;
        };
        let needs_separator = matches!(
            self.transcript.messages.get(index),
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

    pub(crate) fn reset_assistant_stream(&mut self) {
        self.transcript.assistant_stream = StreamingMarkdownAssembler::default();
        self.transcript.assistant_stream_tail = None;
    }

    pub(crate) fn push_proposed_plan_delta(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let last = self.transcript.messages.len().saturating_sub(1);
        if matches!(
            self.transcript.messages.last(),
            Some(ChatMessage::ProposedPlan(_))
        ) {
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

    pub(crate) fn current_turn_proposed_plan(&self) -> Option<String> {
        self.transcript
            .messages
            .get(
                self.transcript
                    .finalized_count
                    .min(self.transcript.messages.len())..,
            )
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
    pub(crate) fn archive_current_plan(&mut self) {
        if let Some((explanation, plan)) = self.take_plan_for_archive() {
            self.push_message(ChatMessage::PlanUpdate { explanation, plan });
        }
    }

    /// Freeze the current turn: everything in `messages` becomes the immutable,
    /// finalized prefix. Called once a turn ends, after trailing reasoning is promoted
    /// and the live plan is archived, so the frozen transcript is in its final shape.
    pub(crate) fn finalize_turn(&mut self) {
        self.transcript.finalized_count = self.transcript.messages.len();
    }

    pub(crate) fn clear_receiving_tool_progress(&mut self) {
        let original_finalized_count = self.transcript.finalized_count;
        let has_receiving_progress = self.transcript.messages[original_finalized_count.min(self.transcript.messages.len())..]
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
}

fn merge_background_task_snapshots(
    background: &[orca_core::task_types::BackgroundTaskSummary],
    focused: &[orca_core::task_types::BackgroundTaskSummary],
) -> Vec<orca_core::task_types::BackgroundTaskSummary> {
    let mut merged = background.to_vec();
    for child_task in focused {
        if let Some(existing) = merged.iter_mut().find(|task| task.id == child_task.id) {
            *existing = child_task.clone();
        } else {
            merged.push(child_task.clone());
        }
    }
    merged
}

fn surface_task_status_label(status: orca_core::task_types::TaskStatus) -> &'static str {
    match status {
        orca_core::task_types::TaskStatus::Queued => "queued",
        orca_core::task_types::TaskStatus::Running => "running",
        orca_core::task_types::TaskStatus::Paused => "paused",
        orca_core::task_types::TaskStatus::Stopping => "stopping",
        orca_core::task_types::TaskStatus::Stopped => "stopped",
        orca_core::task_types::TaskStatus::Completed => "completed",
        orca_core::task_types::TaskStatus::Failed => "failed",
        orca_core::task_types::TaskStatus::ApprovalRequired => "approval required",
        orca_core::task_types::TaskStatus::Cancelled => "cancelled",
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
