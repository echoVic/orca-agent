//! Hosted renderer backend using the standard ACP SDK, never a local runtime host.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::{
    Agent, CancelNotification, Client, ClientCapabilities, ContentBlock, ImageContent,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModelRequest,
    StopReason, ToolCallContent, ToolCallStatus, ToolCallUpdateFields,
};
use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender};
use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::OperationIdAllocator;
use orca_core::config::ReasoningEffort;
use orca_core::conversation::{ImageDetail, ImageInput, ImageSource};
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_runtime::acp::{
    PROJECTION_META,
    client::{Connection, readiness_warnings},
};
use orca_runtime::mentions::MentionBindings;
use orca_runtime::runtime_permission::RuntimePermissionRequestKind;

use crate::action_dispatcher::InteractionResponseAck;
use crate::clipboard_image::{
    MAX_COMPOSER_IMAGE_BYTES, MAX_COMPOSER_IMAGE_COUNT, MAX_COMPOSER_IMAGE_PIXELS,
};
use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::protocol::{
    AcpMetricsSnapshot, TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse,
    TuiPermissionDecision, UserAction,
};
use crate::transcript_state::ChatMessage;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const MAX_PENDING_PERMISSIONS: usize = 128;

#[derive(Clone)]
pub(crate) struct AttachOptions {
    pub socket: PathBuf,
    pub session: String,
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionMeta {
    version: u32,
    item_id: Option<String>,
    offset: Option<u64>,
    phase: Option<String>,
    active: Option<bool>,
    stop_reason: Option<StopReason>,
    error: Option<agent_client_protocol::Error>,
    terminal: Option<orca_runtime::surface::OperationTerminal>,
    usage: Option<orca_runtime::surface::UsageTotals>,
}

#[derive(Clone, Default, PartialEq)]
struct Settings {
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    mode: Option<ApprovalMode>,
}

impl Settings {
    fn update(&mut self, options: Vec<SessionConfigOption>) {
        for option in options {
            let SessionConfigKind::Select(select) = option.kind else {
                continue;
            };
            let value = select.current_value.to_string();
            match option.id.to_string().as_str() {
                "model" => self.model = Some(value),
                "reasoning" => {
                    self.effort = match value.as_str() {
                        "low" => Some(ReasoningEffort::Low),
                        "high" => Some(ReasoningEffort::High),
                        "max" => Some(ReasoningEffort::Max),
                        _ => None,
                    };
                }
                "mode" => self.mode = approval_mode(&value),
                _ => {}
            }
        }
    }

    fn event(&self) -> Option<TuiEvent> {
        Some(TuiEvent::SettingsUpdated {
            model: self.model.clone()?,
            reasoning_effort: self.effort?,
            approval_mode: self.mode?,
        })
    }
}

fn approval_mode(value: &str) -> Option<ApprovalMode> {
    match value {
        "plan" => Some(ApprovalMode::Plan),
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        _ => None,
    }
}

#[derive(Default)]
struct Projection {
    messages: Vec<ChatMessage>,
    indices: HashMap<String, usize>,
    plan_index: Option<usize>,
    image_bytes: usize,
    image_count: usize,
    ready: bool,
    dirty: bool,
    active: bool,
    terminal_seen: bool,
    terminal_diagnostic_seen: bool,
    metrics: AcpMetricsSnapshot,
    metrics_dirty: bool,
    settings: Settings,
    pending_startup_warnings: Vec<String>,
}

impl Projection {
    fn text(&mut self, id: String, offset: usize, text: String, kind: u8) -> Result<(), ()> {
        let key = format!("text:{id}");
        let index = if let Some(index) = self.indices.get(&key) {
            *index
        } else {
            if offset != 0 {
                return Err(());
            }
            let index = self.messages.len();
            self.messages.push(match kind {
                0 => ChatMessage::User(String::new()),
                1 => ChatMessage::Assistant(String::new()),
                _ => ChatMessage::Reasoning(String::new()),
            });
            self.indices.insert(key, index);
            index
        };
        let buffer = match (&mut self.messages[index], kind) {
            (ChatMessage::User(value), 0)
            | (ChatMessage::Assistant(value), 1)
            | (ChatMessage::Reasoning(value), 2) => value,
            _ => return Err(()),
        };
        let existing = buffer.get(offset..).ok_or(())?;
        let overlap = existing.len().min(text.len());
        if existing.get(..overlap).ok_or(())? != text.get(..overlap).ok_or(())? {
            return Err(());
        }
        let suffix = text.get(overlap..).ok_or(())?;
        if !suffix.is_empty() {
            buffer.push_str(suffix);
            self.dirty = true;
        }
        Ok(())
    }

    fn image(&mut self, id: String, image: ImageContent) {
        let key = format!("image:{id}");
        if self.indices.contains_key(&key) {
            return;
        }
        let result = validate_image(&image, self.image_bytes).and_then(|bytes| {
            if self.image_count >= MAX_COMPOSER_IMAGE_COUNT {
                return Err("ACP image replay exceeds the attachment count limit".into());
            }
            let label = format!("[Image #{}]", self.image_count + 1);
            let (_, attachments) = ComposerImageState::restore_from_inputs(
                &label,
                vec![ImageInput {
                    source: ImageSource::Base64 {
                        media_type: image.mime_type,
                        data: image.data,
                    },
                    detail: ImageDetail::High,
                }],
            );
            self.image_bytes += bytes;
            self.image_count += 1;
            Ok(ChatMessage::Image(attachments[0].preview()))
        });
        self.indices.insert(key, self.messages.len());
        self.messages
            .push(result.unwrap_or_else(ChatMessage::Error));
        self.dirty = true;
    }

    fn plan(&mut self, plan: agent_client_protocol::Plan) {
        let message = ChatMessage::PlanUpdate {
            explanation: None,
            plan: plan
                .entries
                .into_iter()
                .map(|entry| PlanItem {
                    step: entry.content,
                    status: match entry.status {
                        agent_client_protocol::PlanEntryStatus::Pending => PlanStatus::Pending,
                        agent_client_protocol::PlanEntryStatus::InProgress => {
                            PlanStatus::InProgress
                        }
                        agent_client_protocol::PlanEntryStatus::Completed => PlanStatus::Completed,
                        _ => PlanStatus::Pending,
                    },
                })
                .collect(),
        };
        // ACP plans are display-only; they do not grant local plan execution.
        if let Some(index) = self.plan_index {
            self.messages[index] = message;
        } else {
            self.plan_index = Some(self.messages.len());
            self.messages.push(message);
        }
        self.dirty = true;
    }

    fn tool(&mut self, id: String, fields: ToolCallUpdateFields) {
        let index = *self.indices.entry(format!("tool:{id}")).or_insert_with(|| {
            self.messages.push(ChatMessage::ToolCall {
                id,
                name: "tool".into(),
                target: None,
                status: "pending".into(),
                output: None,
                diff: None,
                kind: None,
                expanded: false,
            });
            self.messages.len() - 1
        });
        if let ChatMessage::ToolCall {
            name,
            status,
            output,
            ..
        } = &mut self.messages[index]
        {
            if let Some(title) = fields.title {
                *name = title;
            }
            if let Some(value) = fields.status {
                *status = match value {
                    ToolCallStatus::Pending => "pending",
                    ToolCallStatus::InProgress => "running",
                    ToolCallStatus::Completed => "completed",
                    ToolCallStatus::Failed => "failed",
                    _ => "unknown",
                }
                .into();
            }
            if let Some(content) = fields.content {
                *output = Some(
                    content
                        .into_iter()
                        .filter_map(|content| match content {
                            ToolCallContent::Content(content) => match content.content {
                                ContentBlock::Text(text) => Some(text.text),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            self.dirty = true;
        }
    }

    fn usage(&mut self, update: agent_client_protocol::UsageUpdate, meta: &ProjectionMeta) {
        self.metrics.context_used_tokens = usize::try_from(update.used).unwrap_or(usize::MAX);
        self.metrics.context_limit_tokens = usize::try_from(update.size).unwrap_or(usize::MAX);
        if let Some(usage) = &meta.usage {
            self.metrics.usage = orca_core::cost_types::UsageTotals {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_tokens: usage.cache_tokens,
                estimated_cost_usd: usage.estimated_cost_usd_micros as f64 / 1_000_000.0,
            };
        } else if let Some(cost) = update.cost
            && cost.currency == "USD"
            && cost.amount.is_finite()
            && cost.amount >= 0.0
        {
            // Context occupancy is not cumulative token usage.
            self.metrics.usage.estimated_cost_usd = cost.amount;
        }
        self.metrics_dirty = true;
    }
}

struct PendingPermission {
    reply: tokio::sync::oneshot::Sender<RequestPermissionOutcome>,
    options: Vec<PermissionOption>,
}

struct TuiClient {
    events: Sender<TuiEvent>,
    acks: Sender<InteractionResponseAck>,
    ids: Rc<OperationIdAllocator>,
    session: RefCell<Option<SessionId>>,
    projection: RefCell<Projection>,
    pending: RefCell<HashMap<TuiInteractionKey, PendingPermission>>,
    awaiting_result: RefCell<Vec<TuiInteractionKey>>,
    permission_gate: tokio::sync::Mutex<()>,
    permissions_cancelled: Cell<bool>,
    reload: Cell<bool>,
    closed: Cell<bool>,
}

impl TuiClient {
    fn new(
        events: Sender<TuiEvent>,
        acks: Sender<InteractionResponseAck>,
        ids: Rc<OperationIdAllocator>,
    ) -> Self {
        Self {
            events,
            acks,
            ids,
            session: RefCell::new(None),
            projection: RefCell::new(Projection::default()),
            pending: RefCell::new(HashMap::new()),
            awaiting_result: RefCell::new(Vec::new()),
            permission_gate: tokio::sync::Mutex::new(()),
            permissions_cancelled: Cell::new(false),
            reload: Cell::new(false),
            closed: Cell::new(false),
        }
    }

    fn flush(&self) {
        let mut projection = self.projection.borrow_mut();
        if !projection.ready || self.reload.get() {
            return;
        }
        if projection.metrics_dirty {
            projection.metrics_dirty = false;
            let _ = self
                .events
                .send(TuiEvent::AcpMetricsUpdated(projection.metrics.clone()));
        }
        if projection.dirty {
            projection.dirty = false;
            let _ = self.events.send(TuiEvent::AcpTranscriptSynced {
                messages: projection.messages.clone(),
            });
        }
    }

    fn record_error(&self, message: String) {
        let mut projection = self.projection.borrow_mut();
        projection.messages.push(ChatMessage::Error(message));
        projection.dirty = true;
    }

    fn reject_operation(&self, message: String) {
        self.record_error(message.clone());
        let _ = self.events.send(TuiEvent::OperationRejected(message));
        // A denied observer action must not hide genuine remote work.
        if self.projection.borrow().active {
            let _ = self.events.send(TuiEvent::TurnStarted {
                turn: 1,
                task: None,
            });
        }
        self.flush();
    }

    fn reject_submission(&self, submission: Submission, message: String) {
        let _ = self.events.send(TuiEvent::SubmissionRejected {
            queued_id: submission.queued_id,
            prompt: submission.text,
            bindings: submission.bindings,
            images: submission.images,
            message: message.clone(),
        });
        self.record_error(message);
        self.flush();
    }

    fn acknowledge(&self, ack: InteractionResponseAck) {
        if self.acks.try_send(ack).is_err() {
            self.reject_operation("ACP interaction acknowledgement could not be delivered".into());
        }
    }

    fn respond(&self, key: TuiInteractionKey, response: TuiInteractionResponse) {
        let request = self.pending.borrow_mut().remove(&key);
        let Some(request) = request else {
            self.acknowledge(InteractionResponseAck::NoLongerPending {
                key,
                message: "ACP permission is no longer pending".into(),
            });
            return;
        };
        let outcome = match response {
            TuiInteractionResponse::Permission(decision) => {
                permission_outcome(&request.options, decision)
            }
            _ => None,
        };
        let Some(outcome) = outcome else {
            let _ = request.reply.send(RequestPermissionOutcome::Cancelled);
            self.acknowledge(InteractionResponseAck::Failed {
                key,
                message: "The ACP agent did not offer that permission choice; request cancelled"
                    .into(),
            });
            return;
        };
        if request.reply.send(outcome).is_err() {
            self.acknowledge(InteractionResponseAck::NoLongerPending {
                key,
                message: "ACP permission expired before the response was delivered".into(),
            });
        } else {
            // The SDK has no successful response-write receipt (its outgoing
            // broadcast even fires on write errors). Await the prompt result.
            self.awaiting_result.borrow_mut().push(key);
        }
    }

    fn settle_permissions(&self, confirmed: bool, message: &str) {
        let awaiting = std::mem::take(&mut *self.awaiting_result.borrow_mut());
        for key in awaiting {
            self.acknowledge(if confirmed {
                InteractionResponseAck::Committed { key }
            } else {
                InteractionResponseAck::Failed {
                    key,
                    message: message.into(),
                }
            });
        }
    }

    fn cancel_permissions(&self, message: &str) {
        self.permissions_cancelled.set(true);
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        for (key, request) in pending {
            let _ = request.reply.send(RequestPermissionOutcome::Cancelled);
            self.acknowledge(InteractionResponseAck::NoLongerPending {
                key,
                message: message.into(),
            });
        }
    }

    fn finish_turn(&self, status: &str) {
        let mut projection = self.projection.borrow_mut();
        projection.active = false;
        if projection.terminal_seen {
            return;
        }
        projection.terminal_seen = true;
        drop(projection);
        self.flush();
        let _ = self.events.send(TuiEvent::SessionCompleted {
            status: status.into(),
        });
    }

    fn disconnect(&self, prompt: &mut Option<PendingPrompt>) {
        self.closed.set(true);
        self.projection.borrow_mut().active = false;
        self.cancel_permissions("ACP disconnected; permission request was cancelled");
        self.settle_permissions(
            false,
            "ACP disconnected; permission delivery is unconfirmed",
        );
        // This is local cleanup, not a claim that the remote turn stopped.
        let _ = self.events.send(TuiEvent::SessionCompleted {
            status: "disconnected".into(),
        });
        if let Some(prompt) = prompt.take() {
            prompt.task.abort();
            self.reject_submission(prompt.submission,
                "ACP disconnected; prompt delivery is uncertain. Inspect the reloaded session before resubmitting. Nothing was resent.".into());
        }
    }
}

fn permission_outcome(
    options: &[PermissionOption],
    decision: TuiPermissionDecision,
) -> Option<RequestPermissionOutcome> {
    let kind = match decision {
        TuiPermissionDecision::AllowOnce => PermissionOptionKind::AllowOnce,
        TuiPermissionDecision::AllowSession => PermissionOptionKind::AllowAlways,
        TuiPermissionDecision::Deny => PermissionOptionKind::RejectOnce,
    };
    options
        .iter()
        .find(|option| option.kind == kind)
        .map(|option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
        .or_else(|| {
            (decision == TuiPermissionDecision::Deny).then_some(RequestPermissionOutcome::Cancelled)
        })
}

#[async_trait::async_trait(?Send)]
impl Client for TuiClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let _guard = self.permission_gate.lock().await;
        if self.closed.get()
            || self.permissions_cancelled.get()
            || self.session.borrow().as_ref() != Some(&args.session_id)
            || self.awaiting_result.borrow().len() >= MAX_PENDING_PERMISSIONS
        {
            return Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        }
        let key = TuiInteractionKey::new(
            self.ids.allocate(),
            uuid::Uuid::new_v4().to_string(),
            TuiInteractionKind::Permission,
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.borrow_mut().insert(
            key.clone(),
            PendingPermission {
                reply: tx,
                options: args.options,
            },
        );
        let sent = self.events.send(TuiEvent::PermissionApprovalNeeded {
            key: key.clone(),
            tool: args
                .tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "ACP tool permission".into()),
            target: None,
            preview: Some(serde_json::to_string(&args.tool_call).unwrap_or_default()),
            permission_kind: RuntimePermissionRequestKind::CapabilityBoundary,
        });
        let outcome = if sent.is_ok() {
            tokio::time::timeout(Duration::from_secs(110), rx)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(RequestPermissionOutcome::Cancelled)
        } else {
            RequestPermissionOutcome::Cancelled
        };
        let expired = self.pending.borrow_mut().remove(&key).is_some();
        if expired {
            self.acknowledge(InteractionResponseAck::NoLongerPending {
                key,
                message: "ACP permission request expired or was cancelled".into(),
            });
            self.finish_turn("permission expired");
        }
        Ok(RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(
        &self,
        note: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        if let Some(session) = self.session.borrow().as_ref()
            && session != &note.session_id
        {
            return Ok(());
        }
        let meta = match note
            .meta
            .as_ref()
            .and_then(|meta| meta.get(PROJECTION_META))
        {
            Some(value) => match serde_json::from_value::<ProjectionMeta>(value.clone()) {
                Ok(meta) if meta.version == 1 => meta,
                _ => {
                    self.reload.set(true);
                    return Ok(());
                }
            },
            None => ProjectionMeta::default(),
        };
        let mut projection = self.projection.borrow_mut();
        let mut lifecycle = None;
        if let Some(phase) = meta.phase.as_deref() {
            match phase {
                "reset" => {
                    *projection = Projection::default();
                    *self.session.borrow_mut() = Some(note.session_id.clone());
                }
                "ready" if !projection.ready => {
                    projection.ready = true;
                    projection.dirty = true;
                    projection.metrics_dirty = true;
                    projection.messages.push(ChatMessage::System(format!(
                        "ACP session {}",
                        note.session_id
                    )));
                }
                "reload_required" => self.reload.set(true),
                _ => {}
            }
            if let Some(active) = meta.active {
                if projection.ready && active && (!projection.active || phase == "ready") {
                    projection.terminal_seen = false;
                    projection.terminal_diagnostic_seen = false;
                    lifecycle = Some(TuiEvent::TurnStarted {
                        turn: 1,
                        task: None,
                    });
                } else if projection.ready
                    && !active
                    && (phase == "ready" || phase == "terminal")
                    && !projection.terminal_seen
                {
                    projection.terminal_seen = true;
                    let status = if let Some(terminal) = &meta.terminal {
                        if let Some(diagnostic) =
                            crate::diagnostics::TuiDiagnostic::from_surface_terminal(terminal)
                        {
                            projection
                                .messages
                                .push(ChatMessage::Diagnostic(diagnostic));
                            projection.dirty = true;
                            projection.terminal_diagnostic_seen = true;
                        }
                        crate::surface_projection::operation_terminal_status(terminal)
                            .unwrap_or("failed")
                    } else if let Some(error) = &meta.error {
                        projection
                            .messages
                            .push(ChatMessage::Error(error.to_string()));
                        projection.dirty = true;
                        projection.terminal_diagnostic_seen = true;
                        "failed"
                    } else {
                        meta.stop_reason.as_ref().map_or("success", stop_status)
                    };
                    lifecycle = Some(TuiEvent::SessionCompleted {
                        status: status.into(),
                    });
                }
                projection.active = active;
            }
        }
        let settings_before = projection.settings.clone();
        match note.update {
            SessionUpdate::UserMessageChunk(chunk) => {
                self.content(&mut projection, chunk.content, &meta, 0)
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.content(&mut projection, chunk.content, &meta, 1)
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.content(&mut projection, chunk.content, &meta, 2)
            }
            SessionUpdate::ToolCall(call) => {
                projection.tool(
                    call.tool_call_id.to_string(),
                    ToolCallUpdateFields::new()
                        .title(call.title)
                        .status(call.status)
                        .content(call.content),
                );
            }
            SessionUpdate::ToolCallUpdate(call) => {
                projection.tool(call.tool_call_id.to_string(), call.fields);
            }
            SessionUpdate::Plan(plan) => projection.plan(plan),
            SessionUpdate::UsageUpdate(update) => projection.usage(update, &meta),
            SessionUpdate::ConfigOptionUpdate(update) => {
                projection
                    .pending_startup_warnings
                    .extend(readiness_warnings(update.meta.as_ref()));
                projection.settings.update(update.config_options);
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                projection.settings.mode = approval_mode(&update.current_mode_id.to_string());
            }
            _ => {}
        }
        if projection.settings != settings_before
            && let Some(event) = projection.settings.event()
        {
            let _ = self.events.send(event);
        }
        let startup_warnings = if projection.ready {
            std::mem::take(&mut projection.pending_startup_warnings)
        } else {
            Vec::new()
        };
        drop(projection);
        if let Some(event) = lifecycle {
            self.flush();
            let _ = self.events.send(event);
        }
        for warning in startup_warnings {
            let _ = self.events.send(TuiEvent::StartupWarning(warning));
        }
        Ok(())
    }
}

impl TuiClient {
    fn content(
        &self,
        projection: &mut Projection,
        content: ContentBlock,
        meta: &ProjectionMeta,
        kind: u8,
    ) {
        let Some((id, offset)) = meta
            .item_id
            .as_ref()
            .zip(meta.offset.and_then(|offset| usize::try_from(offset).ok()))
        else {
            self.reload.set(true);
            return;
        };
        match content {
            ContentBlock::Text(text) => {
                if projection
                    .text(id.clone(), offset, text.text, kind)
                    .is_err()
                {
                    self.reload.set(true);
                }
            }
            ContentBlock::Image(image) if offset == 0 => projection.image(id.clone(), image),
            _ => {
                projection
                    .messages
                    .push(ChatMessage::Error("Unsupported ACP content block".into()));
                projection.dirty = true;
            }
        }
    }
}

fn stop_status(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "success",
        StopReason::Cancelled => "cancelled",
        StopReason::Refusal => "refused",
        StopReason::MaxTokens => "token limit",
        StopReason::MaxTurnRequests => "turn limit",
        _ => "stopped",
    }
}

struct Submission {
    text: String,
    bindings: MentionBindings,
    images: Vec<ComposerImageAttachment>,
    queued_id: Option<u64>,
}

impl Submission {
    fn text(text: String) -> Self {
        Self {
            text,
            bindings: MentionBindings::default(),
            images: Vec::new(),
            queued_id: None,
        }
    }

    fn content(&self) -> Result<Vec<ContentBlock>, String> {
        if !self.bindings.is_empty() {
            return Err(
                "Bound mentions are not supported by this ACP attachment; input was not sent"
                    .into(),
            );
        }
        if self.images.len() > MAX_COMPOSER_IMAGE_COUNT {
            return Err("ACP prompt exceeds the image attachment count limit".into());
        }
        let text = ComposerImageState::text_without_labels(&self.text, &self.images);
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(text.into());
        }
        let mut bytes = 0;
        for input in ComposerImageState::image_inputs(&self.images) {
            let ImageSource::Base64 { media_type, data } = input.source else {
                return Err(
                    "ACP prompts require inline images; URL/file images were not sent".into(),
                );
            };
            let image = ImageContent::new(data, media_type);
            bytes += validate_image(&image, bytes)?;
            content.push(ContentBlock::Image(image));
        }
        if content.is_empty() {
            return Err("ACP prompt is empty; input was not sent".into());
        }
        Ok(content)
    }
}

fn validate_image(image: &ImageContent, used_bytes: usize) -> Result<usize, String> {
    let remaining = MAX_COMPOSER_IMAGE_BYTES.saturating_sub(used_bytes);
    if image.data.len() > remaining.div_ceil(3).saturating_mul(4) {
        return Err("ACP inline images exceed the 5 MiB limit".into());
    }
    if !matches!(
        image.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err("Unsupported ACP image media type".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|_| "Invalid ACP image base64".to_string())?;
    if bytes.len() > remaining {
        return Err("ACP inline images exceed the 5 MiB limit".into());
    }
    let reader = image::ImageReader::new(io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| "Invalid ACP image".to_string())?;
    if reader
        .format()
        .is_none_or(|format| format.to_mime_type() != image.mime_type)
    {
        return Err("ACP image data does not match its media type".into());
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "Invalid ACP image".to_string())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_COMPOSER_IMAGE_PIXELS
    {
        return Err("ACP image exceeds the preview pixel limit".into());
    }
    Ok(bytes.len())
}

struct PendingPrompt {
    submission: Submission,
    task: tokio::task::JoinHandle<agent_client_protocol::Result<PromptResponse>>,
}

fn submit(
    connection: &Connection,
    id: &SessionId,
    submission: Submission,
    pending: &mut Option<PendingPrompt>,
    client: &TuiClient,
) {
    if pending.is_some() || client.projection.borrow().active {
        client.reject_submission(
            submission,
            "An ACP turn is already running; input was not sent".into(),
        );
        let _ = client.events.send(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        return;
    }
    let content = match submission.content() {
        Ok(content) => content,
        Err(error) => {
            client.reject_submission(submission, error);
            return;
        }
    };
    client.projection.borrow_mut().terminal_seen = false;
    client.permissions_cancelled.set(false);
    let agent = connection.agent.clone();
    let id = id.clone();
    *pending = Some(PendingPrompt {
        submission,
        task: tokio::task::spawn_local(async move {
            agent.prompt(PromptRequest::new(id, content)).await
        }),
    });
}

fn finish_prompt(
    client: &TuiClient,
    submission: Submission,
    result: Result<agent_client_protocol::Result<PromptResponse>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(response)) => {
            client.settle_permissions(true, "");
            client.finish_turn(stop_status(&response.stop_reason));
            if response.stop_reason == StopReason::Refusal {
                client.reject_submission(submission, "ACP agent refused this prompt".into());
            }
        }
        result => {
            let message = match result {
                Ok(Err(error)) => error.to_string(),
                Err(error) => format!("ACP prompt task failed: {error}"),
                _ => unreachable!(),
            };
            client.cancel_permissions("ACP prompt ended before permission was resolved");
            client.settle_permissions(
                false,
                "ACP prompt failed; permission delivery is unconfirmed",
            );
            let (terminal_seen, terminal_diagnostic_seen) = {
                let projection = client.projection.borrow();
                (
                    projection.terminal_seen,
                    projection.terminal_diagnostic_seen,
                )
            };
            if terminal_seen {
                if !terminal_diagnostic_seen {
                    client.record_error(message);
                    client.flush();
                }
                return;
            }
            client.finish_turn("failed");
            client.reject_submission(submission, message);
        }
    }
}

fn reject_action(client: &TuiClient, action: UserAction, message: &str) {
    match action {
        UserAction::Submit(text) => {
            client.reject_submission(Submission::text(text), message.into())
        }
        UserAction::SubmitWithMentions {
            prompt,
            bindings,
            images,
        }
        | UserAction::QueuePrompt {
            prompt,
            bindings,
            images,
        } => {
            client.reject_submission(
                Submission {
                    text: prompt,
                    bindings,
                    images,
                    queued_id: None,
                },
                message.into(),
            );
        }
        UserAction::SubmitQueued {
            id,
            prompt,
            bindings,
            images,
        } => {
            client.reject_submission(
                Submission {
                    text: prompt,
                    bindings,
                    images,
                    queued_id: Some(id),
                },
                message.into(),
            );
        }
        UserAction::PasteImages { request_id, .. } => {
            let _ = client.events.send(TuiEvent::ClipboardImagePasteCompleted {
                request_id,
                result: Err(message.into()),
            });
        }
        UserAction::RespondToInteraction { key, .. } => {
            client.acknowledge(InteractionResponseAck::NoLongerPending {
                key,
                message: message.into(),
            });
        }
        _ => client.reject_operation(message.into()),
    }
}

async fn while_detached<T>(
    future: impl Future<Output = io::Result<T>>,
    client: &TuiClient,
    actions: &Receiver<UserAction>,
    stop: &AtomicBool,
) -> io::Result<T> {
    tokio::pin!(future);
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "ACP client stopped",
            ));
        }
        while let Ok(action) = actions.try_recv() {
            if matches!(action, UserAction::Cancel) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "ACP client stopped",
                ));
            }
            reject_action(
                client,
                action,
                "ACP is disconnected or loading; input was not sent",
            );
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

fn retry_delay(attempts: &mut u32) -> Option<Duration> {
    if *attempts >= MAX_RECONNECT_ATTEMPTS {
        return None;
    }
    let delay = Duration::from_millis(250 * (1_u64 << *attempts));
    *attempts += 1;
    Some(delay)
}

async fn control_request(
    agent: &impl Agent,
    session: SessionId,
    action: UserAction,
) -> Result<(), String> {
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        match action {
            UserAction::SetModel(value) => {
                let (model, effort) = match crate::slash_command_actions::decode_settings_intent(
                    &value,
                ) {
                    Some(intent) if intent.approval_mode.is_none() => (
                        intent
                            .model
                            .ok_or_else(|| "Select a model for the ACP attachment".to_string())?,
                        intent.reasoning_effort,
                    ),
                    Some(_) => return Err(
                        "Approval mode and plan changes are not available on this ACP attachment"
                            .into(),
                    ),
                    None => {
                        orca_core::model::validate_model(&value)
                            .map_err(|error| error.to_string())?;
                        (
                            orca_core::model::canonical_model_name(&value).to_string(),
                            None,
                        )
                    }
                };
                agent
                    .set_session_model(SetSessionModelRequest::new(session.clone(), model))
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(effort) = effort {
                    agent
                        .set_session_config_option(SetSessionConfigOptionRequest::new(
                            session,
                            "reasoning",
                            effort.as_str(),
                        ))
                        .await
                        .map_err(|error| {
                            format!("ACP model changed, but reasoning update failed: {error}")
                        })?;
                }
                Ok(())
            }
            UserAction::Interrupt => agent
                .cancel(CancelNotification::new(session))
                .await
                .map_err(|error| error.to_string()),
            _ => Err("Unsupported ACP control request".into()),
        }
    })
    .await
    .map_err(|_| "ACP control request timed out; outcome is unconfirmed".to_string())?
}

pub(crate) fn run(
    options: AttachOptions,
    cwd: PathBuf,
    actions: Receiver<UserAction>,
    events: Sender<TuiEvent>,
    acks: Sender<InteractionResponseAck>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&runtime, async {
        let mut selector = options.session;
        let ids = Rc::new(OperationIdAllocator::new());
        let mut attempts = 0;
        while !stop.load(Ordering::Acquire) {
            let client = Rc::new(TuiClient::new(events.clone(), acks.clone(), ids.clone()));
            if selector != "new" {
                *client.session.borrow_mut() = Some(SessionId::new(selector.clone()));
            }
            let capabilities = ClientCapabilities::new().meta(serde_json::Map::from_iter([
                (PROJECTION_META.into(), serde_json::json!({"version": 1})),
            ]));
            let attached = while_detached(async {
                let connection = Connection::connect(&options.socket, client.clone(), capabilities).await?;
                let attached = connection.attach_with_metadata(&cwd, &selector).await?;
                tokio::time::timeout(CONTROL_TIMEOUT, async {
                    while !client.projection.borrow().ready {
                        if connection.is_closed() || client.reload.get() {
                            return Err(io::Error::other("ACP snapshot delivery was interrupted"));
                        }
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                    Ok(())
                }).await??;
                Ok((connection, attached))
            }, &client, &actions, &stop).await;
            let (connection, attached) = match attached {
                Ok(attached) => attached,
                Err(error) => {
                    client.disconnect(&mut None);
                    if error.kind() == io::ErrorKind::Interrupted {
                        return Ok(());
                    }
                    // Never repeat session/new when its response might be lost.
                    let delay = (selector != "new").then(|| retry_delay(&mut attempts)).flatten();
                    let Some(delay) = delay else {
                        client.reject_operation(format!("ACP attachment failed: {error}"));
                        return Err(error);
                    };
                    let _ = events.send(TuiEvent::Notice(format!(
                        "ACP reconnect attempt {attempts}/{MAX_RECONNECT_ATTEMPTS}: {error}"
                    )));
                    if while_detached(async { tokio::time::sleep(delay).await; Ok(()) },
                        &client, &actions, &stop).await.is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
            };
            for warning in attached.startup_warnings {
                let _ = events.send(TuiEvent::StartupWarning(warning));
            }
            let id = attached.session_id;
            selector = id.to_string();
            *client.session.borrow_mut() = Some(id.clone());
            client.flush();
            let connected_at = tokio::time::Instant::now();
            let mut prompt: Option<PendingPrompt> = None;
            let mut control: Option<tokio::task::JoinHandle<Result<(), String>>> = None;
            loop {
                if connected_at.elapsed() >= Duration::from_secs(10) {
                    attempts = 0;
                }
                if stop.load(Ordering::Acquire) {
                    client.disconnect(&mut prompt);
                    if let Some(control) = control { control.abort(); }
                    return Ok(());
                }
                if connection.is_closed() || client.reload.get() {
                    client.disconnect(&mut prompt);
                    if let Some(control) = control.take() { control.abort(); }
                    let Some(delay) = retry_delay(&mut attempts) else {
                        client.reject_operation("ACP reconnect limit reached; attach again explicitly".into());
                        return Err(io::Error::other("ACP reconnect limit reached"));
                    };
                    let _ = events.send(TuiEvent::Notice("ACP disconnected; reloading the authoritative session. The last prompt will not be resent.".into()));
                    drop(connection);
                    if while_detached(async { tokio::time::sleep(delay).await; Ok(()) },
                        &client, &actions, &stop).await.is_err()
                    {
                        return Ok(());
                    }
                    break;
                }
                if prompt.as_ref().is_some_and(|prompt| prompt.task.is_finished()) {
                    let pending = prompt.take().expect("finished prompt");
                    finish_prompt(&client, pending.submission, pending.task.await);
                }
                if control.as_ref().is_some_and(|task| task.is_finished()) {
                    match control.take().expect("finished control").await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => client.reject_operation(error),
                        Err(error) => client.reject_operation(format!("ACP control task failed: {error}")),
                    }
                }
                while let Ok(action) = actions.try_recv() {
                    match action {
                        UserAction::Cancel => {
                            client.disconnect(&mut prompt);
                            if let Some(control) = control { control.abort(); }
                            return Ok(());
                        }
                        UserAction::RespondToInteraction { key, response } => {
                            client.respond(key, response);
                        }
                        UserAction::Submit(text) =>
                            submit(&connection, &id, Submission::text(text), &mut prompt, &client),
                        UserAction::SubmitWithMentions { prompt: text, bindings, images } =>
                            submit(&connection, &id, Submission { text, bindings, images, queued_id: None }, &mut prompt, &client),
                        action @ (UserAction::SetModel(_) | UserAction::Interrupt) => {
                            if control.is_some() {
                                client.reject_operation("An ACP settings/cancel request is already pending".into());
                                continue;
                            }
                            if matches!(action, UserAction::Interrupt) {
                                client.cancel_permissions("ACP permission cancelled by user");
                            }
                            let agent = connection.agent.clone();
                            let session = id.clone();
                            control = Some(tokio::task::spawn_local(async move {
                                control_request(agent.as_ref(), session, action).await
                            }));
                        }
                        UserAction::PasteImages { request_id, request } => {
                            let events = events.clone();
                            tokio::task::spawn_blocking(move || {
                                let result = crate::clipboard_image::read_image_request(request)
                                    .map_err(|error| error.to_string());
                                let _ = events.send(TuiEvent::ClipboardImagePasteCompleted { request_id, result });
                            });
                        }
                        action => reject_action(&client, action,
                            "This action is unavailable on the ACP attachment; prompts, inline images, permissions, model selection and interrupt are supported."),
                    }
                }
                client.flush();
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{
        ContentChunk, Cost, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionInfoUpdate,
        ToolCallUpdate, UsageUpdate,
    };
    use image::ImageEncoder as _;
    use serde_json::json;

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    fn client() -> (
        Rc<TuiClient>,
        Receiver<TuiEvent>,
        Receiver<InteractionResponseAck>,
    ) {
        let (events, event_rx) = crossbeam_channel::unbounded();
        let (acks, ack_rx) = crossbeam_channel::unbounded();
        let client = Rc::new(TuiClient::new(
            events,
            acks,
            Rc::new(OperationIdAllocator::new()),
        ));
        *client.session.borrow_mut() = Some(SessionId::new(SESSION));
        (client, event_rx, ack_rx)
    }

    // These are the negotiated observer.rs v1 fields, carried on ordinary SDK
    // notifications. No test-only protocol or extra metadata is introduced.
    fn note(update: SessionUpdate, meta: serde_json::Value) -> SessionNotification {
        SessionNotification::new(SESSION, update)
            .meta(serde_json::Map::from_iter([(PROJECTION_META.into(), meta)]))
    }

    fn phase(name: &str, active: bool) -> SessionNotification {
        note(
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
            json!({"version": 1, "phase": name, "active": active}),
        )
    }

    fn text(id: &str, offset: u64, value: &str) -> SessionNotification {
        note(
            SessionUpdate::AgentMessageChunk(ContentChunk::new(value.into())),
            json!({"version": 1, "itemId": id, "offset": offset}),
        )
    }

    fn permission(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SESSION,
            ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new().title("Write a file")),
            options,
        )
    }

    fn permission_options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("once", "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "session",
                "Allow for this session",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
        ]
    }

    async fn permission_key(events: &Receiver<TuiEvent>) -> TuiInteractionKey {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(event) = events.try_recv() {
                    if let TuiEvent::PermissionApprovalNeeded {
                        key,
                        permission_kind,
                        ..
                    } = event
                    {
                        assert_eq!(
                            permission_kind,
                            RuntimePermissionRequestKind::CapabilityBoundary
                        );
                        assert_eq!(key.kind, TuiInteractionKind::Permission);
                        return key;
                    }
                    panic!("unexpected event while waiting for permission: {event:?}");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission event")
    }

    fn png() -> ImageContent {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[20, 40, 60, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        ImageContent::new(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "image/png",
        )
    }

    fn image_submission() -> Submission {
        let image = png();
        let (text, images) = ComposerImageState::restore_from_inputs(
            "inspect [Image #7]",
            vec![ImageInput {
                source: ImageSource::Base64 {
                    media_type: image.mime_type,
                    data: image.data,
                },
                detail: ImageDetail::High,
            }],
        );
        Submission {
            text,
            images,
            bindings: MentionBindings::default(),
            queued_id: None,
        }
    }

    struct Renderer {
        state: crate::types::AppState,
        textarea: tui_textarea::TextArea<'static>,
        vim: crate::vim::VimState,
        theme: crate::theme::Theme,
        actions: Sender<UserAction>,
        pending: crate::bridge::PendingWorkflowNotifications,
        presentation: crate::terminal_presentation::TerminalPresentation,
    }

    impl Renderer {
        fn new() -> Self {
            let (actions, _) = crossbeam_channel::unbounded();
            Self {
                state: crate::types::AppState::new(
                    actions.clone(),
                    "test".into(),
                    "mock".into(),
                    "/tmp".into(),
                ),
                textarea: tui_textarea::TextArea::default(),
                vim: crate::vim::VimState::new(false),
                theme: crate::theme::Theme::named(orca_core::config::ThemeName::Dark),
                actions,
                pending: crate::bridge::PendingWorkflowNotifications::new(),
                presentation: crate::terminal_presentation::TerminalPresentation::new(
                    false,
                    crate::terminal_presentation::TerminalPresentationProfile {
                        osc9_supported: false,
                        tmux_passthrough: false,
                    },
                ),
            }
        }

        fn drain(&mut self, events: &Receiver<TuiEvent>) {
            for event in events.try_iter() {
                crate::runtime_event_actions::handle_runtime_event(
                    event,
                    &mut self.state,
                    &self.actions,
                    &self.pending,
                    &mut self.textarea,
                    &mut self.vim,
                    &self.theme,
                    &mut self.presentation,
                );
            }
        }
    }

    #[tokio::test]
    async fn permissions_preserve_scope_and_wait_for_prompt_result() {
        let (client, events, acks) = client();
        let mut previous = None;
        for (decision, option) in [
            (TuiPermissionDecision::AllowOnce, "once"),
            (TuiPermissionDecision::AllowSession, "session"),
            (TuiPermissionDecision::Deny, "deny"),
        ] {
            let (response, key) = tokio::join!(
                client.request_permission(permission(permission_options())),
                async {
                    let key = permission_key(&events).await;
                    client.respond(key.clone(), TuiInteractionResponse::Permission(decision));
                    assert!(
                        acks.is_empty(),
                        "local oneshot delivery is not transport commitment"
                    );
                    key
                },
            );
            assert_eq!(
                response.unwrap().outcome,
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option),)
            );
            if let Some(previous) = previous {
                assert_ne!(key.operation_id, previous);
            }
            previous = Some(key.operation_id);
            assert!(client.pending.borrow().is_empty());
            assert!(
                acks.is_empty(),
                "returning a reverse response is not a write receipt"
            );
            finish_prompt(
                &client,
                Submission::text("prompt".into()),
                Ok(Ok(PromptResponse::new(StopReason::EndTurn))),
            );
            assert_eq!(
                acks.try_recv().unwrap(),
                InteractionResponseAck::Committed { key }
            );
            events.try_iter().for_each(drop);
        }
    }

    #[tokio::test]
    async fn permission_missing_scope_and_stale_keys_fail_closed() {
        let (client, events, acks) = client();
        let options = vec![PermissionOption::new(
            "once",
            "Once",
            PermissionOptionKind::AllowOnce,
        )];
        let (response, key) = tokio::join!(client.request_permission(permission(options)), async {
            let key = permission_key(&events).await;
            client.respond(
                key.clone(),
                TuiInteractionResponse::Permission(TuiPermissionDecision::AllowSession),
            );
            key
        },);
        assert_eq!(
            response.unwrap().outcome,
            RequestPermissionOutcome::Cancelled
        );
        assert!(
            matches!(acks.try_recv().unwrap(), InteractionResponseAck::Failed { key: actual, .. } if actual == key)
        );
        assert!(client.pending.borrow().is_empty());
        client.respond(key.clone(), TuiInteractionResponse::Approval(true));
        assert!(matches!(acks.try_recv().unwrap(),
            InteractionResponseAck::NoLongerPending { key: actual, .. } if actual == key));
        assert_eq!(
            permission_outcome(&[], TuiPermissionDecision::Deny),
            Some(RequestPermissionOutcome::Cancelled)
        );
    }

    #[tokio::test]
    async fn permission_ids_survive_reconnect_allocator() {
        let (first, events, _) = client();
        let (events2, rx2) = crossbeam_channel::unbounded();
        let (acks2, _) = crossbeam_channel::unbounded();
        let second = TuiClient::new(events2, acks2, first.ids.clone());
        *second.session.borrow_mut() = Some(SessionId::new(SESSION));
        let mut keys = Vec::new();
        for (client, events) in [(first.as_ref(), &events), (&second, &rx2)] {
            let (response, key) = tokio::join!(
                client.request_permission(permission(permission_options())),
                async {
                    let key = permission_key(events).await;
                    client.respond(
                        key.clone(),
                        TuiInteractionResponse::Permission(TuiPermissionDecision::AllowOnce),
                    );
                    key
                },
            );
            assert!(matches!(
                response.unwrap().outcome,
                RequestPermissionOutcome::Selected(_)
            ));
            keys.push(key);
        }
        assert_ne!(keys[0].operation_id, keys[1].operation_id);
    }

    #[test]
    fn utf8_overlaps_are_idempotent_and_conflicts_do_not_mutate() {
        let mut projection = Projection::default();
        for (offset, value) in [
            (0, "a\u{e9}"),
            (1, "\u{e9}\u{1f680}"),
            (0, "a\u{e9}"),
            (3, "\u{1f680}!"),
            (1, "\u{e9}\u{1f680}!"),
        ] {
            projection
                .text("answer".into(), offset, value.into(), 1)
                .unwrap();
        }
        assert!(matches!(&projection.messages[..],
            [ChatMessage::Assistant(value)] if value == "a\u{e9}\u{1f680}!"));
        for (offset, value) in [(2, "x"), (1, "wrong"), (100, "gap")] {
            assert!(
                projection
                    .text("answer".into(), offset, value.into(), 1)
                    .is_err()
            );
        }
        assert!(
            projection
                .text("missing".into(), 4, "gap".into(), 1)
                .is_err()
        );
        assert_eq!(projection.messages.len(), 1);
        assert!(matches!(&projection.messages[0],
            ChatMessage::Assistant(value) if value == "a\u{e9}\u{1f680}!"));
    }

    #[tokio::test]
    async fn text_gaps_and_invalid_offsets_request_reload() {
        for bad in [
            text("answer", 9, "gap"),
            text("answer", 2, "not a character boundary"),
            text("answer", 1, "conflict"),
            note(
                SessionUpdate::AgentMessageChunk(ContentChunk::new("missing offset".into())),
                json!({"version": 1, "itemId": "answer"}),
            ),
        ] {
            let (client, events, _) = client();
            client
                .session_notification(phase("reset", false))
                .await
                .unwrap();
            client
                .session_notification(text("answer", 0, "a\u{e9}"))
                .await
                .unwrap();
            client
                .session_notification(phase("ready", false))
                .await
                .unwrap();
            events.try_iter().for_each(drop);
            client.session_notification(bad).await.unwrap();
            client.flush();
            assert!(client.reload.get());
            assert!(events.is_empty(), "a corrupt replica must not be published");
        }
    }

    #[tokio::test]
    async fn snapshot_reset_replaces_rows_and_resets_usage() {
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        for _ in 0..2 {
            client
                .session_notification(phase("reset", false))
                .await
                .unwrap();
            client
                .session_notification(text("answer", 0, "one answer"))
                .await
                .unwrap();
            client
                .session_notification(text("answer", 0, "one answer"))
                .await
                .unwrap();
            client
                .session_notification(note(
                    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Image(png()))),
                    json!({"version": 1, "itemId": "user:image:1", "offset": 0}),
                ))
                .await
                .unwrap();
            client
                .session_notification(phase("ready", false))
                .await
                .unwrap();
            renderer.drain(&events);
            assert_eq!(
                renderer
                    .state
                    .transcript
                    .messages
                    .iter()
                    .filter(|row| matches!(row, ChatMessage::Assistant(_)))
                    .count(),
                1
            );
            assert_eq!(
                renderer
                    .state
                    .transcript
                    .messages
                    .iter()
                    .filter(|row| matches!(row, ChatMessage::Image(_)))
                    .count(),
                1
            );
            assert_eq!(renderer.state.transcript.messages.len(), 3);
        }
        client
            .session_notification(SessionNotification::new(
                SESSION,
                SessionUpdate::UsageUpdate(UsageUpdate::new(900, 1000)),
            ))
            .await
            .unwrap();
        client.flush();
        renderer.drain(&events);
        assert_eq!(renderer.state.context_used_tokens(), 900);
        client
            .session_notification(phase("reset", false))
            .await
            .unwrap();
        client.flush();
        assert!(
            events.is_empty(),
            "reset waits for the complete replacement"
        );
        client
            .session_notification(phase("ready", false))
            .await
            .unwrap();
        renderer.drain(&events);
        assert_eq!(renderer.state.context_used_tokens(), 0);
        assert_eq!(renderer.state.usage().input_tokens, 0);
        assert_eq!(renderer.state.transcript.messages.len(), 1);
    }

    #[tokio::test]
    async fn standard_plan_settings_and_usage_update_existing_owners() {
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        client
            .session_notification(phase("reset", true))
            .await
            .unwrap();
        client
            .session_notification(SessionNotification::new(
                SESSION,
                SessionUpdate::Plan(Plan::new(vec![
                    PlanEntry::new("read", PlanEntryPriority::High, PlanEntryStatus::Completed),
                    PlanEntry::new(
                        "change",
                        PlanEntryPriority::Medium,
                        PlanEntryStatus::InProgress,
                    ),
                    PlanEntry::new("verify", PlanEntryPriority::Low, PlanEntryStatus::Pending),
                ])),
            ))
            .await
            .unwrap();
        let options = [
            ("model", "deepseek-v4-pro"),
            ("mode", "suggest"),
            ("reasoning", "high"),
        ]
        .into_iter()
        .map(|(key, value)| {
            SessionConfigOption::select(
                key,
                key,
                value,
                Vec::<agent_client_protocol::SessionConfigSelectOption>::new(),
            )
        })
        .collect();
        client
            .session_notification(SessionNotification::new(
                SESSION,
                SessionUpdate::ConfigOptionUpdate(
                    agent_client_protocol::ConfigOptionUpdate::new(options).meta(
                        serde_json::Map::from_iter([(
                            orca_runtime::acp::READINESS_META.to_string(),
                            json!({
                                "version": 1,
                                "startupWarnings": ["shell unavailable"],
                            }),
                        )]),
                    ),
                ),
            ))
            .await
            .unwrap();
        let usage = orca_runtime::surface::UsageTotals {
            input_tokens: 9000,
            output_tokens: 700,
            cache_tokens: 6000,
            estimated_cost_usd_micros: 125_000,
        };
        client
            .session_notification(note(
                SessionUpdate::UsageUpdate(
                    UsageUpdate::new(4096, 128000).cost(Cost::new(0.125, "USD")),
                ),
                json!({"version": 1, "usage": usage}),
            ))
            .await
            .unwrap();
        client
            .session_notification(phase("ready", true))
            .await
            .unwrap();
        renderer.drain(&events);
        assert_eq!(renderer.state.model_name, "deepseek-v4-pro");
        assert_eq!(renderer.state.reasoning_effort, ReasoningEffort::High);
        assert_eq!(renderer.state.approval_mode, ApprovalMode::Suggest);
        assert_eq!(renderer.state.context_used_tokens(), 4096);
        assert_eq!(renderer.state.context_limit_tokens(), 128000);
        assert_eq!(renderer.state.usage().input_tokens, 9000);
        assert_eq!(renderer.state.usage().output_tokens, 700);
        assert_eq!(renderer.state.usage().cache_tokens, 6000);
        assert_eq!(renderer.state.usage().estimated_cost_usd, 0.125);
        assert_eq!(renderer.state.status, crate::types::AppStatus::Running);
        assert!(renderer.state.transcript.messages.iter().any(
            |message| matches!(message, ChatMessage::System(text) if text == "shell unavailable")
        ));
        assert!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .any(|message| matches!(
                    message, ChatMessage::PlanUpdate { plan, .. }
                        if plan.iter().map(|entry| entry.status.clone()).collect::<Vec<_>>()
                            == [PlanStatus::Completed, PlanStatus::InProgress, PlanStatus::Pending]
                ))
        );
        client
            .session_notification(SessionNotification::new(
                SESSION,
                SessionUpdate::Plan(Plan::new(vec![PlanEntry::new(
                    "done",
                    PlanEntryPriority::High,
                    PlanEntryStatus::Completed,
                )])),
            ))
            .await
            .unwrap();
        client.flush();
        renderer.drain(&events);
        assert_eq!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .filter(|row| matches!(row, ChatMessage::PlanUpdate { .. }))
                .count(),
            1
        );
        assert!(renderer.state.current_goal().is_none());
    }

    #[tokio::test]
    async fn standard_usage_never_invents_token_totals() {
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        client
            .session_notification(phase("ready", false))
            .await
            .unwrap();
        client
            .session_notification(SessionNotification::new(
                SESSION,
                SessionUpdate::UsageUpdate(
                    UsageUpdate::new(800, 1000).cost(Cost::new(0.25, "USD")),
                ),
            ))
            .await
            .unwrap();
        client.flush();
        renderer.drain(&events);
        assert_eq!(renderer.state.usage().input_tokens, 0);
        assert_eq!(renderer.state.usage().output_tokens, 0);
        assert_eq!(renderer.state.context_used_tokens(), 800);
        assert_eq!(renderer.state.usage().estimated_cost_usd, 0.25);
        for cost in [
            Cost::new(999.0, "EUR"),
            Cost::new(-1.0, "USD"),
            Cost::new(f64::NAN, "USD"),
        ] {
            client
                .session_notification(SessionNotification::new(
                    SESSION,
                    SessionUpdate::UsageUpdate(UsageUpdate::new(500, 1000).cost(cost)),
                ))
                .await
                .unwrap();
        }
        client.flush();
        renderer.drain(&events);
        assert_eq!(renderer.state.context_used_tokens(), 500);
        assert_eq!(renderer.state.usage().estimated_cost_usd, 0.25);
    }

    #[tokio::test]
    async fn terminal_errors_stop_spinner_and_preserve_diagnostics() {
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        client
            .session_notification(phase("ready", true))
            .await
            .unwrap();
        client
            .session_notification(text("answer", 0, "partial answer"))
            .await
            .unwrap();
        client.session_notification(note(
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
            json!({"version": 1, "phase": "terminal", "active": false,
                "error": agent_client_protocol::Error::internal_error().data("provider unavailable")}),
        )).await.unwrap();
        renderer.drain(&events);
        assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
        assert!(renderer.state.approval_dialog.is_none());
        assert!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .any(|row| matches!(
                    row, ChatMessage::Error(error) if error.contains("provider unavailable")
                ))
        );
        assert_eq!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .filter(|row| { matches!(row, ChatMessage::Error(_) | ChatMessage::Diagnostic(_)) })
                .count(),
            1,
            "status fallback must not duplicate the projected ACP error"
        );
        client
            .session_notification(text("answer", 0, "partial answer"))
            .await
            .unwrap();
        client.flush();
        renderer.drain(&events);
        assert!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .any(|row| matches!(row, ChatMessage::Error(_)))
        );
    }

    #[tokio::test]
    async fn typed_terminal_metadata_preserves_failure_class_and_action() {
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        client
            .session_notification(phase("ready", true))
            .await
            .unwrap();
        let terminal = orca_runtime::surface::OperationTerminal::Failed {
            class: orca_runtime::surface::FailureClass::Provider,
            message: orca_runtime::surface::SafeDiagnosticText::try_new(
                "DeepSeek returned 503 Service Unavailable",
            )
            .unwrap(),
        };
        client
            .session_notification(note(
                SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
                json!({
                    "version": 1,
                    "phase": "terminal",
                    "active": false,
                    "terminal": terminal,
                }),
            ))
            .await
            .unwrap();

        renderer.drain(&events);

        assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
        let diagnostics = renderer
            .state
            .transcript
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Diagnostic(diagnostic) => Some(diagnostic),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), "provider.failed");
        assert!(diagnostics[0].detail().contains("503 Service Unavailable"));
        assert!(diagnostics[0].action().is_some());
    }

    #[test]
    fn confirmed_terminal_error_does_not_restore_or_duplicate_the_prompt() {
        let (client, events, _) = client();
        {
            let mut projection = client.projection.borrow_mut();
            projection.terminal_seen = true;
            projection.terminal_diagnostic_seen = true;
        }

        finish_prompt(
            &client,
            Submission::text("already delivered".to_string()),
            Ok(Err(
                agent_client_protocol::Error::internal_error().data("exact terminal cause")
            )),
        );

        assert!(
            events.try_iter().all(|event| !matches!(
                event,
                TuiEvent::SubmissionRejected { .. }
                    | TuiEvent::Error(_)
                    | TuiEvent::Diagnostic(_)
                    | TuiEvent::SessionCompleted { .. }
            )),
            "a confirmed terminal already owns the visible outcome"
        );
    }

    #[tokio::test]
    async fn prompt_rejection_restores_mentions_and_images_without_losing_history() {
        use orca_runtime::mentions::{MentionBinding, MentionFileKind, MentionTarget};
        let (client, events, _) = client();
        let mut renderer = Renderer::new();
        client
            .session_notification(phase("reset", false))
            .await
            .unwrap();
        client
            .session_notification(text("old-answer", 0, "accepted history"))
            .await
            .unwrap();
        client
            .session_notification(phase("ready", false))
            .await
            .unwrap();
        renderer.drain(&events);
        let mut submission = image_submission();
        submission.text = "inspect @file.rs [Image #7]".into();
        submission.bindings = MentionBindings::from_bindings(
            &submission.text,
            vec![MentionBinding {
                start: 8,
                end: 16,
                visible: "@file.rs".into(),
                target: MentionTarget::File {
                    root: PathBuf::from("/workspace"),
                    path: "file.rs".into(),
                    kind: MentionFileKind::File,
                },
            }],
        );
        let original = submission.text.clone();
        let bindings = submission.bindings.clone();
        let images = submission.images.clone();
        renderer
            .state
            .push_user_message_with_images(original.clone(), &images);
        renderer.state.enter_running();
        let error = submission.content().unwrap_err();
        client.reject_submission(submission, error);
        renderer.drain(&events);
        assert_eq!(
            crate::composer_textarea::textarea_text(&renderer.textarea),
            original
        );
        assert_eq!(renderer.state.mention_bindings, bindings);
        assert_eq!(
            renderer
                .state
                .composer_images
                .attachments_for_text(&original),
            images
        );
        assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
        assert!(
            renderer
                .state
                .transcript
                .messages
                .iter()
                .any(|row| matches!(
                    row, ChatMessage::Assistant(value) if value == "accepted history"
                ))
        );
        assert!(
            !renderer
                .state
                .transcript
                .messages
                .iter()
                .any(|row| matches!(row, ChatMessage::User(_)))
        );
    }

    #[test]
    fn image_prompt_and_replay_use_standard_blocks_and_bounded_previews() {
        let submission = image_submission();
        let content = submission.content().unwrap();
        assert!(
            matches!(&content[..], [ContentBlock::Text(text), ContentBlock::Image(image)]
            if text.text == "inspect" && image.mime_type == "image/png")
        );
        let ContentBlock::Image(image) = &content[1] else {
            unreachable!()
        };
        let mut projection = Projection::default();
        projection.image("user:image:1".into(), image.clone());
        projection.image("user:image:1".into(), image.clone());
        assert_eq!(projection.messages.len(), 1);
        assert!(
            matches!(&projection.messages[0], ChatMessage::Image(preview)
            if preview.width == 1 && preview.height == 1 && preview.encoded.is_some())
        );
        assert!(validate_image(image, MAX_COMPOSER_IMAGE_BYTES).is_err());
        assert!(validate_image(&ImageContent::new("invalid!", "image/png"), 0).is_err());
        assert!(validate_image(&ImageContent::new(image.data.clone(), "image/jpeg"), 0).is_err());
        assert!(
            validate_image(
                &ImageContent::new(
                    "A".repeat(MAX_COMPOSER_IMAGE_BYTES.div_ceil(3) * 4 + 1),
                    "image/png"
                ),
                0
            )
            .is_err()
        );
        let mut image_only = image_submission();
        image_only.text = "[Image #7]".into();
        assert!(matches!(
            &image_only.content().unwrap()[..],
            [ContentBlock::Image(_)]
        ));
        projection.image_bytes = MAX_COMPOSER_IMAGE_BYTES;
        projection.image("user:image:2".into(), image.clone());
        assert!(matches!(
            projection.messages.last(),
            Some(ChatMessage::Error(_))
        ));
    }

    #[tokio::test]
    async fn disconnect_clears_permissions_and_never_resends_prompt() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, events, acks) = client();
                let (response, ()) = tokio::join!(
                    client.request_permission(permission(permission_options())),
                    async {
                        let _key = permission_key(&events).await;
                        let mut prompt = Some(PendingPrompt {
                            submission: Submission::text("uncertain input".into()),
                            task: tokio::task::spawn_local(std::future::pending()),
                        });
                        client.disconnect(&mut prompt);
                        assert!(prompt.is_none());
                    },
                );
                assert_eq!(
                    response.unwrap().outcome,
                    RequestPermissionOutcome::Cancelled
                );
                assert!(client.pending.borrow().is_empty());
                assert!(client.awaiting_result.borrow().is_empty());
                assert!(matches!(
                    acks.try_recv().unwrap(),
                    InteractionResponseAck::NoLongerPending { .. }
                ));
                assert!(events.try_iter().any(|event| matches!(event,
                TuiEvent::SubmissionRejected { prompt, message, .. }
                    if prompt == "uncertain input" && message.contains("Nothing was resent"))));
                let response = client
                    .request_permission(permission(permission_options()))
                    .await
                    .unwrap();
                assert_eq!(response.outcome, RequestPermissionOutcome::Cancelled);
                client
                    .session_notification(text("late", 0, "old transport"))
                    .await
                    .unwrap();
                assert!(
                    client
                        .projection
                        .borrow()
                        .messages
                        .iter()
                        .all(|row| !matches!(row, ChatMessage::Assistant(_)))
                );
            })
            .await;
    }

    #[tokio::test]
    async fn detached_actions_are_rejected_not_queued_and_retry_is_bounded() {
        let (client, events, _) = client();
        let (actions, receiver) = crossbeam_channel::unbounded();
        actions
            .send(UserAction::Submit("not delivered".into()))
            .unwrap();
        let stopped = AtomicBool::new(false);
        while_detached(async { Ok(()) }, &client, &receiver, &stopped)
            .await
            .unwrap();
        assert!(receiver.is_empty());
        assert!(matches!(events.try_recv().unwrap(),
            TuiEvent::SubmissionRejected { prompt, .. } if prompt == "not delivered"));
        let mut attempts = 0;
        for millis in [250, 500, 1000, 2000, 4000] {
            assert_eq!(
                retry_delay(&mut attempts),
                Some(Duration::from_millis(millis))
            );
        }
        assert_eq!(retry_delay(&mut attempts), None);
        assert_eq!(attempts, MAX_RECONNECT_ATTEMPTS);
        actions.send(UserAction::Cancel).unwrap();
        let error = while_detached(
            std::future::pending::<io::Result<()>>(),
            &client,
            &receiver,
            &stopped,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    struct WirePeer {
        read: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        write: tokio::net::unix::OwnedWriteHalf,
    }

    #[cfg(unix)]
    impl WirePeer {
        fn bind() -> (tempfile::TempDir, PathBuf, tokio::net::UnixListener) {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::Builder::new()
                .prefix("orca-acp-tui-")
                .tempdir_in("/tmp")
                .unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let path = directory.path().join("test.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            (directory, path, listener)
        }

        async fn accept(listener: tokio::net::UnixListener) -> Self {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = stream.into_split();
            let mut peer = Self {
                read: tokio::io::BufReader::new(read),
                write,
            };
            let initialize = peer.recv().await;
            assert_eq!(initialize["method"], "initialize");
            let _: agent_client_protocol::InitializeRequest =
                serde_json::from_value(initialize["params"].clone()).unwrap();
            peer.send(json!({
                "jsonrpc": "2.0", "id": initialize["id"],
                "result": agent_client_protocol::InitializeResponse::new(agent_client_protocol::ProtocolVersion::V1)
            })).await;
            peer
        }

        async fn recv(&mut self) -> serde_json::Value {
            use tokio::io::AsyncBufReadExt as _;
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(3), self.read.read_line(&mut line))
                .await
                .expect("ACP peer request timeout")
                .unwrap();
            assert_ne!(read, 0, "ACP peer closed unexpectedly");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, value: serde_json::Value) {
            use tokio::io::AsyncWriteExt as _;
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_permissions_commit_only_after_peer_acceptance_and_prompt_result() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_directory, path, listener) = WirePeer::bind();
                let received = Rc::new(tokio::sync::Notify::new());
                let release = Rc::new(tokio::sync::Notify::new());
                let received_peer = received.clone();
                let release_peer = release.clone();
                let peer = tokio::task::spawn_local(async move {
                    let mut peer = WirePeer::accept(listener).await;
                    for (index, option) in ["once", "session", "deny", "session"]
                        .into_iter()
                        .enumerate()
                    {
                        let request = peer.recv().await;
                        assert_eq!(request["method"], "session/prompt");
                        let prompt: PromptRequest =
                            serde_json::from_value(request["params"].clone()).unwrap();
                        assert_eq!(prompt.session_id, SessionId::new(SESSION));
                        assert!(matches!(&prompt.prompt[..],
                        [ContentBlock::Text(text), ContentBlock::Image(image)]
                            if text.text == "inspect" && image == &png()));
                        peer.send(json!({
                            "jsonrpc": "2.0", "id": 100 + index,
                            "method": "session/request_permission",
                            "params": permission(permission_options())
                        }))
                        .await;
                        let answer = peer.recv().await;
                        assert_eq!(answer["id"], json!(100 + index));
                        let answer: RequestPermissionResponse =
                            serde_json::from_value(answer["result"].clone()).unwrap();
                        assert_eq!(
                            answer.outcome,
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                option
                            ))
                        );
                        received_peer.notify_one();
                        release_peer.notified().await;
                        if index == 3 {
                            // The peer read the permission but disconnects without a
                            // prompt result. The client must report uncertainty.
                            return;
                        }
                        peer.send(json!({
                            "jsonrpc": "2.0", "id": request["id"],
                            "result": PromptResponse::new(StopReason::EndTurn)
                        }))
                        .await;
                    }
                });
                let (client, events, acks) = client();
                let connection =
                    Connection::connect(&path, client.clone(), ClientCapabilities::new())
                        .await
                        .unwrap();
                for (index, decision) in [
                    TuiPermissionDecision::AllowOnce,
                    TuiPermissionDecision::AllowSession,
                    TuiPermissionDecision::Deny,
                    TuiPermissionDecision::AllowSession,
                ]
                .into_iter()
                .enumerate()
                {
                    events.try_iter().for_each(drop);
                    let mut pending = None;
                    submit(
                        &connection,
                        &SessionId::new(SESSION),
                        image_submission(),
                        &mut pending,
                        &client,
                    );
                    let key = permission_key(&events).await;
                    client.respond(key.clone(), TuiInteractionResponse::Permission(decision));
                    tokio::time::timeout(Duration::from_secs(3), received.notified())
                        .await
                        .unwrap();
                    assert!(
                        acks.is_empty(),
                        "even peer receipt is not the prompt result"
                    );
                    release.notify_one();
                    let pending = pending.take().unwrap();
                    let result = tokio::time::timeout(Duration::from_secs(3), pending.task)
                        .await
                        .unwrap();
                    finish_prompt(&client, pending.submission, result);
                    if index < 3 {
                        assert_eq!(
                            acks.try_recv().unwrap(),
                            InteractionResponseAck::Committed { key }
                        );
                    } else {
                        assert!(matches!(acks.try_recv().unwrap(),
                        InteractionResponseAck::Failed { key: actual, message }
                            if actual == key && message.contains("unconfirmed")));
                        let mut renderer = Renderer::new();
                        renderer.state.enter_running();
                        renderer.drain(&events);
                        assert_eq!(
                            crate::composer_textarea::textarea_text(&renderer.textarea),
                            "inspect [Image #7]"
                        );
                        assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
                        assert_eq!(
                            renderer
                                .state
                                .composer_images
                                .attachments_for_text("inspect [Image #7]")
                                .len(),
                            1
                        );
                    }
                }
                peer.await.unwrap();
            })
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_sequential_permissions_surface_before_same_prompt_completes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_directory, path, listener) = WirePeer::bind();
                let release = Rc::new(tokio::sync::Notify::new());
                let release_peer = release.clone();
                let peer = tokio::task::spawn_local(async move {
                    let mut peer = WirePeer::accept(listener).await;
                    let prompt = peer.recv().await;
                    assert_eq!(prompt["method"], "session/prompt");
                    for (index, expected) in ["once", "session"].into_iter().enumerate() {
                        peer.send(json!({
                            "jsonrpc": "2.0", "id": 200 + index,
                            "method": "session/request_permission",
                            "params": RequestPermissionRequest::new(SESSION,
                                ToolCallUpdate::new(format!("tool-{index}"),
                                    ToolCallUpdateFields::new().title(format!("Tool {index}"))),
                                permission_options())
                        }))
                        .await;
                        let answer = peer.recv().await;
                        assert_eq!(answer["id"], json!(200 + index));
                        let response: RequestPermissionResponse =
                            serde_json::from_value(answer["result"].clone()).unwrap();
                        assert_eq!(
                            response.outcome,
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                expected
                            ))
                        );
                    }
                    release_peer.notified().await;
                    peer.send(json!({"jsonrpc": "2.0", "id": prompt["id"],
                    "result": PromptResponse::new(StopReason::EndTurn)}))
                        .await;
                });
                let (client, events, acks) = client();
                let connection =
                    Connection::connect(&path, client.clone(), ClientCapabilities::new())
                        .await
                        .unwrap();
                let (actions, action_rx) = crossbeam_channel::unbounded();
                let mut renderer = Renderer::new();
                let mut pending = None;
                submit(
                    &connection,
                    &SessionId::new(SESSION),
                    Submission::text("two tools".into()),
                    &mut pending,
                    &client,
                );
                let mut keys = Vec::new();
                for option in [
                    crate::types::ApprovalOption::Once,
                    crate::types::ApprovalOption::AlwaysTool,
                ] {
                    let event = tokio::time::timeout(Duration::from_secs(3), async {
                        loop {
                            if let Ok(event) = events.try_recv() {
                                break event;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    let TuiEvent::PermissionApprovalNeeded { key, .. } = &event else {
                        panic!("expected next permission before prompt completion: {event:?}");
                    };
                    keys.push(key.clone());
                    renderer.state.update(event);
                    assert_eq!(
                        renderer.state.status,
                        crate::types::AppStatus::WaitingApproval
                    );
                    assert!(renderer.state.approval_dialog.is_some());
                    crate::approval_actions::resolve_approval_option(
                        &mut renderer.state,
                        &actions,
                        option,
                    );
                    assert!(
                        renderer.state.approval_dialog.is_none(),
                        "sending the choice closes the modal"
                    );
                    assert!(renderer.state.interaction.pending_submission.is_none());
                    let UserAction::RespondToInteraction { key, response } =
                        action_rx.try_recv().unwrap()
                    else {
                        panic!("approval action");
                    };
                    client.respond(key, response);
                    assert!(acks.is_empty());
                    assert!(!pending.as_ref().unwrap().task.is_finished());
                }
                assert_eq!(client.awaiting_result.borrow().len(), 2);
                assert_ne!(keys[0], keys[1]);
                release.notify_one();
                let prompt = pending.take().unwrap();
                let result = tokio::time::timeout(Duration::from_secs(3), prompt.task)
                    .await
                    .unwrap();
                finish_prompt(&client, prompt.submission, result);
                assert_eq!(
                    acks.try_iter().collect::<Vec<_>>(),
                    keys.into_iter()
                        .map(|key| InteractionResponseAck::Committed { key })
                        .collect::<Vec<_>>()
                );
                renderer.drain(&events);
                assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
                peer.await.unwrap();
            })
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_picker_uses_standard_requests_and_reports_denial() {
        tokio::task::LocalSet::new().run_until(async {
            let (_directory, path, listener) = WirePeer::bind();
            let peer = tokio::task::spawn_local(async move {
                let mut peer = WirePeer::accept(listener).await;
                let model = peer.recv().await;
                assert_eq!(model["method"], "session/set_model");
                assert_eq!(model["params"]["modelId"], "deepseek-v4-pro");
                assert_eq!(model["params"]["sessionId"], SESSION);
                peer.send(json!({"jsonrpc": "2.0", "id": model["id"], "result": {}})).await;
                let reasoning = peer.recv().await;
                assert_eq!(reasoning["method"], "session/set_config_option");
                assert_eq!(reasoning["params"]["configId"], "reasoning");
                assert_eq!(reasoning["params"]["value"], "high");
                peer.send(json!({"jsonrpc": "2.0", "id": reasoning["id"], "result": {"configOptions": []}})).await;
                let denied = peer.recv().await;
                assert_eq!(denied["method"], "session/set_model");
                assert_eq!(denied["params"]["modelId"], orca_core::model::FLASH_MODEL);
                peer.send(json!({"jsonrpc": "2.0", "id": denied["id"],
                    "error": agent_client_protocol::Error::invalid_request().data("settings lease denied")
                })).await;
            });
            let (client, events, _) = client();
            let connection = Connection::connect(&path, client.clone(), ClientCapabilities::new()).await.unwrap();
            let intent = crate::slash_command_actions::encode_settings_intent(
                Some("deepseek-v4-pro"), Some(ReasoningEffort::High), None,
            );
            control_request(connection.agent.as_ref(), SessionId::new(SESSION),
                UserAction::SetModel(intent)).await.unwrap();
            let error = control_request(connection.agent.as_ref(), SessionId::new(SESSION),
                UserAction::SetModel(orca_core::model::LEGACY_FLASH_MODEL.into())).await.unwrap_err();
            assert!(error.contains("settings lease denied"));
            let mut renderer = Renderer::new();
            renderer.state.enter_running();
            client.reject_operation(error);
            renderer.drain(&events);
            assert_eq!(renderer.state.status, crate::types::AppStatus::Idle);
            assert_eq!(renderer.state.model_name, "mock", "denial must not publish optimistic settings");
            let mode = crate::slash_command_actions::encode_settings_intent(None, None, Some(ApprovalMode::Plan));
            assert!(control_request(connection.agent.as_ref(), SessionId::new(SESSION),
                UserAction::SetModel(mode)).await.unwrap_err().contains("not available"));
            peer.await.unwrap();
        }).await;
    }
}
