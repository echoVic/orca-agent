//! Stateless hosted session snapshot and history event shaping.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::approval_types::{ApprovalDecision, ApprovalResolution};
use orca_core::config::HistoryMode;
use orca_core::conversation::Message;
use orca_core::event_schema::{EventEnvelope, EventType};
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_mcp::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
use orca_runtime::history;
use orca_runtime::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequestHandler, RuntimeUserInputHandler,
    RuntimeUserInputRequest,
};
use orca_runtime::protocol::{PermissionGrantScope, PermissionResponseDecision};
use orca_runtime::runtime_host::{PromptQueueInteractionHandlers, RuntimeThreadHandle};
use orca_runtime::runtime_permission::{RuntimePermissionRequest, RuntimePermissionResponse};
use orca_runtime::surface::RuntimeSurfaceHostHandle;
use std::io;

use crate::attachment_routing::send_attached_event;
use crate::composer_images::ComposerImageState;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::surface_actions::TuiSurfaceActions;
use crate::surface_projection::{SessionProjectionPresentation, SurfaceProjectionState};
use crate::types::{
    ChatMessage, SessionAttachmentId, TuiEvent, TuiInteractionKind, TuiInteractionResponse,
    TuiMcpElicitationMode,
};

struct QueueApprovalHandler {
    control: TuiSurfaceTaskControl,
}

impl RuntimeApprovalHandler for QueueApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &orca_core::approval_types::ApprovalRequest,
        request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        let tool = approval
            .tool
            .as_deref()
            .unwrap_or_else(|| request.name.as_str())
            .to_string();
        let response = self.control.await_queue_interaction(
            approval.id.clone(),
            TuiInteractionKind::Approval,
            |key| TuiEvent::ApprovalNeeded {
                key,
                tool,
                target: approval.target.clone().or_else(|| request.target.clone()),
                preview: approval
                    .preview
                    .clone()
                    .or_else(|| Some(approval.description.clone())),
            },
        )?;
        match response {
            TuiInteractionResponse::Approval(approved) => Ok(ApprovalResolution {
                id: approval.id.clone(),
                decision: if approved {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny
                },
                reason: if approved {
                    "approved in TUI".to_string()
                } else {
                    "denied in TUI".to_string()
                },
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "queued approval response kind did not match the request",
            )),
        }
    }
}

struct QueuePermissionHandler {
    control: TuiSurfaceTaskControl,
}

impl RuntimePermissionRequestHandler for QueuePermissionHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let response = self.control.await_queue_interaction(
            request.id.clone(),
            TuiInteractionKind::Permission,
            |key| TuiEvent::PermissionApprovalNeeded {
                key,
                tool: request.id.clone(),
                target: None,
                preview: request.reason.clone(),
                permission_kind: permission_kind(request),
            },
        )?;
        let approved = match response {
            TuiInteractionResponse::Permission(approved) => approved,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "queued permission response kind did not match the request",
                ));
            }
        };
        Ok(RuntimePermissionResponse {
            decision: if approved {
                PermissionResponseDecision::Allow
            } else {
                PermissionResponseDecision::Deny
            },
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

struct QueueUserInputHandler {
    control: TuiSurfaceTaskControl,
}

impl RuntimeUserInputHandler for QueueUserInputHandler {
    fn request_user_input(&self, request: &RuntimeUserInputRequest) -> io::Result<Option<String>> {
        let response = self.control.await_queue_interaction(
            request.id.clone(),
            TuiInteractionKind::UserInput,
            |key| TuiEvent::UserInputRequested {
                key,
                question: request.question.clone(),
                choices: request.choices.clone(),
            },
        )?;
        match response {
            TuiInteractionResponse::UserInput(answer) => Ok(Some(answer)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "queued user-input response kind did not match the request",
            )),
        }
    }
}

struct QueueMcpElicitationHandler {
    control: TuiSurfaceTaskControl,
}

impl McpElicitationHandler for QueueMcpElicitationHandler {
    fn handle_elicitation(
        &self,
        request: McpElicitationRequest,
    ) -> Result<McpElicitationResponse, String> {
        let mode = match request.mode {
            McpElicitationMode::Form => TuiMcpElicitationMode::Form,
            McpElicitationMode::Url => TuiMcpElicitationMode::Url,
        };
        let response = self
            .control
            .await_queue_interaction(
                request.id.clone(),
                TuiInteractionKind::McpElicitation,
                |key| TuiEvent::McpElicitationRequested {
                    key,
                    server_name: request.server_name.clone(),
                    mode,
                    message: request.message.clone(),
                    url: request.url.clone(),
                    requested_schema_json: request.requested_schema.as_ref().map(|value| {
                        serde_json::to_string(value).expect("MCP schema is serializable")
                    }),
                },
            )
            .map_err(|error| error.to_string())?;
        match response {
            TuiInteractionResponse::McpElicitation {
                accepted: true,
                content_json,
            } => {
                let content = serde_json::from_str(content_json.as_deref().unwrap_or("{}"))
                    .map_err(|error| format!("invalid queued MCP elicitation content: {error}"))?;
                Ok(McpElicitationResponse::accept(content))
            }
            TuiInteractionResponse::McpElicitation {
                accepted: false, ..
            } => Ok(McpElicitationResponse::decline()),
            _ => Err("queued MCP elicitation response kind did not match the request".to_string()),
        }
    }
}

fn permission_kind(
    request: &RuntimePermissionRequest,
) -> orca_runtime::runtime_permission::RuntimePermissionRequestKind {
    if request
        .permissions
        .network
        .as_ref()
        .is_some_and(|network| network.enabled == Some(true) || !network.domains.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock;
    }
    if request
        .permissions
        .file_system
        .as_ref()
        .and_then(|filesystem| filesystem.write.as_ref())
        .is_some_and(|paths| !paths.is_empty())
    {
        return orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite;
    }
    orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry
}

fn runtime_event_to_tui(event: &EventEnvelope) -> Option<TuiEvent> {
    let string = |key: &str| event.payload.get(key).and_then(|value| value.as_str());
    match event.event_type {
        EventType::TurnStarted => Some(TuiEvent::TurnStarted {
            turn: event
                .payload
                .get("turn")
                .and_then(|value| value.as_u64())
                .unwrap_or_default() as u32,
            task: None,
        }),
        EventType::AssistantReasoningDelta => {
            Some(TuiEvent::ReasoningDelta(string("text")?.to_string()))
        }
        EventType::AssistantMessageDelta => {
            Some(TuiEvent::MessageDelta(string("text")?.to_string()))
        }
        EventType::ContextCompactionStarted => Some(TuiEvent::CompactionStarted),
        EventType::ContextCompacted => Some(TuiEvent::Compacted {
            before_messages: event
                .payload
                .get("before_messages")
                .and_then(|value| value.as_u64())
                .unwrap_or_default() as usize,
            after_messages: event
                .payload
                .get("after_messages")
                .and_then(|value| value.as_u64())
                .unwrap_or_default() as usize,
            reason: string("reason").unwrap_or_default().to_string(),
            strategy: string("strategy").unwrap_or_default().to_string(),
            collapsed_messages: event
                .payload
                .get("collapsed_messages")
                .and_then(|value| value.as_u64())
                .unwrap_or_default() as usize,
            status_text: string("status_text").unwrap_or_default().to_string(),
        }),
        EventType::ModelResponseCompleted => serde_json::from_value::<
            orca_core::thread_item_projection::CompletedModelResponse,
        >(event.payload.clone())
        .ok()
        .map(|response| {
            TuiEvent::AssistantResponseCompleted(
                response.assistant_content,
                response.assistant_reasoning,
            )
        }),
        EventType::ToolCallProgress => Some(TuiEvent::ToolCallProgress {
            id: string("id")?.to_string(),
            name: string("name").map(str::to_string),
            arguments_bytes: event
                .payload
                .get("arguments_bytes")
                .and_then(|value| value.as_u64())
                .unwrap_or_default() as usize,
        }),
        EventType::ToolOutputDelta => Some(TuiEvent::ToolOutputDelta {
            id: string("id")?.to_string(),
            chunk: string("chunk")?.to_string(),
        }),
        EventType::ToolCallRequested => Some(TuiEvent::ToolRequested {
            id: string("id")?.to_string(),
            name: string("name")?.to_string(),
            target: string("target").map(str::to_string),
        }),
        EventType::ToolCallCompleted => Some(TuiEvent::ToolCompleted {
            id: string("id")?.to_string(),
            name: string("name")?.to_string(),
            status: string("status").unwrap_or("completed").to_string(),
            output: string("output")
                .or_else(|| string("error"))
                .unwrap_or_default()
                .to_string(),
            diff: string("diff").map(str::to_string),
            kind: string("kind").map(str::to_string),
        }),
        EventType::SubagentStarted => Some(TuiEvent::SubagentStarted {
            id: string("id")?.to_string(),
            description: string("description").unwrap_or("subagent").to_string(),
        }),
        EventType::SubagentProgress => Some(TuiEvent::SubagentProgress {
            id: string("id")?.to_string(),
            activity: string("activity").unwrap_or("running").to_string(),
            turn: event
                .payload
                .get("turn")
                .and_then(|value| value.as_u64())
                .map(|turn| turn as u32),
            usage: event
                .payload
                .get("usage")
                .cloned()
                .filter(|value| !value.is_null())
                .and_then(|value| serde_json::from_value(value).ok()),
        }),
        EventType::SubagentCompleted => Some(TuiEvent::SubagentCompleted {
            id: string("id")?.to_string(),
            description: string("description").unwrap_or("subagent").to_string(),
            status: string("status").unwrap_or("failed").to_string(),
            output: string("output").map(str::to_string),
            error: string("error").map(str::to_string),
        }),
        EventType::PlanUpdated => serde_json::from_value(event.payload.clone()).ok().map(
            |plan: orca_core::plan_types::UpdatePlanArgs| TuiEvent::PlanUpdated {
                explanation: plan.explanation,
                plan: plan.plan,
            },
        ),
        EventType::SessionCompleted => Some(TuiEvent::SessionCompleted {
            status: string("status").unwrap_or("failed").to_string(),
        }),
        EventType::Error => Some(TuiEvent::Error(
            string("message")
                .unwrap_or("runtime operation failed")
                .to_string(),
        )),
        _ => None,
    }
}

fn start_prompt_queue_watcher(
    mut queue_updates: tokio::sync::watch::Receiver<
        orca_runtime::prompt_queue::PromptQueueSnapshot,
    >,
    event_sink: Arc<Mutex<Option<mpsc::Sender<TuiEvent>>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) {
    let watcher_event_sink = event_sink.clone();
    let watcher_stop = stop.clone();
    let watcher = Box::new(move || {
        while !watcher_stop.load(std::sync::atomic::Ordering::Acquire) {
            match queue_updates.has_changed() {
                Ok(true) => {
                    let snapshot = queue_updates.borrow_and_update().clone();
                    let queue_events = watcher_event_sink
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if let Some(queue_events) = queue_events
                        && queue_events
                            .send(TuiEvent::PromptQueueUpdated(snapshot))
                            .is_err()
                    {
                        watcher_stop.store(true, std::sync::atomic::Ordering::Release);
                        break;
                    }
                }
                Ok(false) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    });
    if let Err(error) = spawn(watcher) {
        stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(event_tx) = event_sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to start prompt queue watcher: {error}"
            )));
        }
    }
}

pub(crate) fn announce_runtime_ready(
    thread: &RuntimeThreadHandle,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: &TuiSurfaceTaskControl,
) {
    let watcher_stop = control.bind_prompt_queue_runtime(thread.clone(), event_tx);
    thread.set_prompt_queue_interaction_handlers(PromptQueueInteractionHandlers {
        approval: Some(Arc::new(QueueApprovalHandler {
            control: control.clone(),
        })),
        permission: Some(Arc::new(QueuePermissionHandler {
            control: control.clone(),
        })),
        user_input: Some(Arc::new(QueueUserInputHandler {
            control: control.clone(),
        })),
        mcp_elicitation: Some(Arc::new(QueueMcpElicitationHandler {
            control: control.clone(),
        })),
    });
    let runtime_events = event_tx.clone();
    thread.set_prompt_queue_event_observer(Arc::new(move |event: &EventEnvelope| {
        if let Some(event) = runtime_event_to_tui(event) {
            let _ = runtime_events.send(event);
        }
        Ok(())
    }));
    let _ = event_tx.send(TuiEvent::MentionRuntimeReady(thread.typed_surface()));
    if let Ok(snapshot) = thread.prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::List) {
        let _ = event_tx.send(TuiEvent::PromptQueueUpdated(snapshot));
    }
    if let Some(watcher_stop) = watcher_stop {
        start_prompt_queue_watcher(
            thread.subscribe_prompt_queue(),
            control.prompt_queue_event_sender(),
            watcher_stop,
            |watcher| {
                std::thread::Builder::new()
                    .name("orca-tui-prompt-queue".to_string())
                    .spawn(watcher)
                    .map(|_| ())
            },
        );
    }
    let actions = TuiSurfaceActions::new(thread.typed_surface());
    match actions.read_snapshot() {
        Ok(snapshot) => {
            let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(
                SurfaceProjectionState::from_surface_snapshot(&snapshot),
            )));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to project the active conversation: {error}"
            )));
        }
    }
}

pub(crate) fn read_hosted_projection_batch(
    thread: &RuntimeThreadHandle,
) -> Result<(SurfaceProjectionState, Vec<ChatMessage>), String> {
    TuiSurfaceActions::new(thread.typed_surface())
        .read_snapshot()
        .map(|snapshot| {
            let projection = SurfaceProjectionState::from_surface_snapshot(&snapshot);
            let mut messages =
                crate::surface_projection::history_messages_from_surface_snapshot(&snapshot);
            if let Some(session_id) = thread.session_id()
                && let Ok(transcript) = load_saved_history_fallback(session_id)
            {
                messages = attach_history_images(messages, &transcript.messages);
            }
            (projection, messages)
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn project_hosted_thread_attached(
    projection: SurfaceProjectionState,
    messages: Vec<ChatMessage>,
    attachment: SessionAttachmentId,
    root_event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    send_attached_event(
        root_event_tx,
        attachment,
        TuiEvent::SessionProjectionReset(Box::new(projection)),
    )
    .map_err(|error| error.to_string())?;
    send_attached_event(
        root_event_tx,
        attachment,
        TuiEvent::HistoryLoaded {
            messages,
            plan: None,
            label: "Inherited conversation context.".to_string(),
        },
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn emit_typed_history_snapshot(
    thread: &RuntimeThreadHandle,
    mode: &HistoryMode,
    session_presentation: Option<SessionProjectionPresentation>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    let actions = TuiSurfaceActions::new(thread.typed_surface());
    let snapshot = actions.read_snapshot().map_err(|error| error.to_string())?;
    let mut messages = crate::surface_projection::history_messages_from_surface_snapshot(&snapshot);
    let mut plan = if snapshot.plan.items.is_empty() && snapshot.plan.explanation.is_none() {
        None
    } else {
        Some((
            snapshot
                .plan
                .explanation
                .as_ref()
                .map(|text| text.as_str().to_string()),
            snapshot
                .plan
                .items
                .iter()
                .map(|item| PlanItem {
                    step: item.step.as_str().to_string(),
                    status: match item.status {
                        orca_runtime::surface::SurfacePlanStatus::Pending => PlanStatus::Pending,
                        orca_runtime::surface::SurfacePlanStatus::InProgress => {
                            PlanStatus::InProgress
                        }
                        orca_runtime::surface::SurfacePlanStatus::Completed => {
                            PlanStatus::Completed
                        }
                    },
                })
                .collect(),
        ))
    };
    if messages.is_empty()
        && let HistoryMode::Resume(selector) | HistoryMode::Fork(selector) = mode
    {
        let transcript = load_saved_history_fallback(selector)?;
        messages = transcript
            .messages
            .into_iter()
            .flat_map(chat_messages_from_history)
            .collect();
        if plan.is_none() {
            plan = transcript.plan;
        }
    } else if let HistoryMode::Resume(selector) | HistoryMode::Fork(selector) = mode
        && let Ok(transcript) = load_saved_history_fallback(selector)
    {
        messages = attach_history_images(messages, &transcript.messages);
    }
    let label = if matches!(mode, HistoryMode::Fork(_)) {
        "Forked saved conversation."
    } else {
        "Resumed saved conversation."
    };
    event_tx
        .send(TuiEvent::HistoryLoaded {
            messages,
            plan,
            label: label.to_string(),
        })
        .map_err(|error| error.to_string())?;
    event_tx
        .send(TuiEvent::SurfaceProjectionSynced(Box::new(
            match session_presentation {
                Some(presentation) => SurfaceProjectionState::from_surface_snapshot(&snapshot)
                    .with_session_presentation(presentation),
                None => SurfaceProjectionState::from_surface_snapshot(&snapshot),
            },
        )))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn attach_history_images(projected: Vec<ChatMessage>, history: &[Message]) -> Vec<ChatMessage> {
    let image_groups = history
        .iter()
        .filter_map(|message| match message {
            Message::User {
                content, images, ..
            } => Some((content.as_str(), images.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut user_index = 0usize;
    let mut enriched = Vec::with_capacity(projected.len());
    for message in projected {
        let is_user = matches!(message, ChatMessage::User(_));
        let visible_text = match &message {
            ChatMessage::User(text) => Some(text.clone()),
            _ => None,
        };
        enriched.push(message);
        if !is_user {
            continue;
        }
        if let Some((history_text, images)) = image_groups.get(user_index)
            && !images.is_empty()
        {
            let visible = visible_text.as_deref().unwrap_or(history_text);
            let (_, attachments) = ComposerImageState::restore_from_inputs(visible, images.clone());
            enriched.extend(
                attachments
                    .iter()
                    .map(|attachment| ChatMessage::Image(attachment.preview())),
            );
        }
        user_index = user_index.saturating_add(1);
    }
    enriched
}

pub(crate) fn load_saved_history_fallback(
    selector: &str,
) -> Result<history::SessionTranscript, String> {
    RuntimeSurfaceHostHandle::load_saved_session(selector)
        .map_err(|error| format!("failed to load saved conversation {selector}: {error}"))
}

pub(crate) fn typed_history_startup_eligible(
    mode: &HistoryMode,
    _preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
) -> bool {
    let HistoryMode::Resume(selector) = mode else {
        return false;
    };
    selector == "latest" || looks_like_uuid_session_id(selector)
}

pub(crate) fn looks_like_uuid_session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub(crate) fn emit_empty_history_snapshot(event_tx: &mpsc::Sender<TuiEvent>, label: &str) {
    let _ = event_tx.send(TuiEvent::HistoryLoaded {
        messages: Vec::new(),
        plan: None,
        label: label.to_string(),
    });
}

#[cfg(test)]
pub(crate) fn chat_message_from_history(message: Message) -> Option<ChatMessage> {
    chat_messages_from_history(message).into_iter().next()
}

pub(crate) fn chat_messages_from_history(message: Message) -> Vec<ChatMessage> {
    match message {
        Message::System { .. } => Vec::new(),
        Message::User {
            content, images, ..
        } => {
            let (visible, attachments) = ComposerImageState::restore_from_inputs(&content, images);
            let mut messages = vec![ChatMessage::User(visible)];
            messages.extend(
                attachments
                    .iter()
                    .map(|attachment| ChatMessage::Image(attachment.preview())),
            );
            messages
        }
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                vec![ChatMessage::Assistant(content)]
            } else if let Some(reasoning) = reasoning_content.filter(|text| !text.trim().is_empty())
            {
                vec![ChatMessage::Reasoning(reasoning)]
            } else if !tool_calls.is_empty() {
                let names = tool_calls
                    .iter()
                    .map(|tool| tool.function_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                vec![ChatMessage::System(format!(
                    "Previous assistant requested tools: {names}"
                ))]
            } else {
                Vec::new()
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            terminal,
            ..
        } => {
            let status = terminal
                .as_ref()
                .map(|terminal| terminal.status.as_str())
                .unwrap_or("completed")
                .to_string();
            let kind = terminal
                .as_ref()
                .and_then(|terminal| serde_json::to_value(terminal.kind).ok())
                .and_then(|value| value.as_str().map(str::to_string));
            let mut output = content;
            if output.is_empty()
                && let Some(error) = terminal
                    .as_ref()
                    .and_then(|terminal| terminal.error.as_ref())
            {
                output = error.clone();
            }
            if status == "indeterminate" && !output.contains("Inspect external state") {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("State is unknown. Inspect external state before retrying.");
            }
            vec![ChatMessage::ToolCall {
                id: tool_call_id.clone(),
                name: format!("tool:{tool_call_id}"),
                target: None,
                status,
                output: (!output.is_empty()).then_some(output),
                diff: None,
                kind,
                expanded: false,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use orca_core::config::HistoryMode;
    use orca_core::event_schema::{EventEnvelope, EventType};

    use super::*;

    #[test]
    fn prompt_queue_watcher_spawn_failure_emits_error() {
        let (_queue_tx, queue_updates) =
            tokio::sync::watch::channel(orca_runtime::prompt_queue::PromptQueueSnapshot::default());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let event_sink = Arc::new(Mutex::new(Some(event_tx.clone())));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        start_prompt_queue_watcher(queue_updates, event_sink, stop, |_| {
            Err(std::io::Error::other("injected spawn failure"))
        });

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Error(message))
                if message == "failed to start prompt queue watcher: injected spawn failure"
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn runtime_queue_event_projection_preserves_incremental_output() {
        let event = EventEnvelope {
            version: "1".to_string(),
            run_id: "run".to_string(),
            seq: 1,
            timestamp_ms: 0,
            event_type: EventType::AssistantMessageDelta,
            payload: serde_json::json!({"text": "hello"}),
        };
        assert!(matches!(
            runtime_event_to_tui(&event),
            Some(TuiEvent::MessageDelta(text)) if text == "hello"
        ));

        let failed_tool = EventEnvelope {
            event_type: EventType::ToolCallCompleted,
            payload: serde_json::json!({
                "id": "tool-1",
                "name": "shell",
                "status": "failed",
                "error": "permission denied"
            }),
            ..event.clone()
        };
        assert!(matches!(
            runtime_event_to_tui(&failed_tool),
            Some(TuiEvent::ToolCompleted { output, .. }) if output == "permission denied"
        ));

        let completed_response_payload = serde_json::to_value(
            orca_core::thread_item_projection::CompletedModelResponse::new(
                orca_core::thread_item_projection::ModelResponseIdentity::new(
                    orca_core::thread_identity::TurnId::new(),
                ),
                Some("final answer".to_string()),
                Some("final reasoning".to_string()),
                Vec::new(),
            ),
        )
        .expect("completed model response should serialize");
        let completed_response = EventEnvelope {
            event_type: EventType::ModelResponseCompleted,
            payload: completed_response_payload,
            ..event
        };
        assert!(matches!(
            runtime_event_to_tui(&completed_response),
            Some(TuiEvent::AssistantResponseCompleted(Some(message), Some(reasoning)))
                if message == "final answer" && reasoning == "final reasoning"
        ));
    }

    #[test]
    fn runtime_queue_event_projection_preserves_subagents_and_compaction() {
        let base = EventEnvelope {
            version: "1".to_string(),
            run_id: "run".to_string(),
            seq: 1,
            timestamp_ms: 0,
            event_type: EventType::SubagentStarted,
            payload: serde_json::json!({
                "id": "agent-1",
                "description": "inspect files"
            }),
        };
        assert!(matches!(
            runtime_event_to_tui(&base),
            Some(TuiEvent::SubagentStarted { id, description })
                if id == "agent-1" && description == "inspect files"
        ));

        let progress = EventEnvelope {
            event_type: EventType::SubagentProgress,
            payload: serde_json::json!({
                "id": "agent-1",
                "description": "inspect files",
                "activity": "reading",
                "turn": 2,
                "usage": null
            }),
            ..base.clone()
        };
        assert!(matches!(
            runtime_event_to_tui(&progress),
            Some(TuiEvent::SubagentProgress { id, activity, turn, usage })
                if id == "agent-1" && activity == "reading" && turn == Some(2) && usage.is_none()
        ));

        let completed = EventEnvelope {
            event_type: EventType::SubagentCompleted,
            payload: serde_json::json!({
                "id": "agent-1",
                "description": "inspect files",
                "status": "success",
                "output": "done",
                "error": null
            }),
            ..base.clone()
        };
        assert!(matches!(
            runtime_event_to_tui(&completed),
            Some(TuiEvent::SubagentCompleted { id, status, output, error, .. })
                if id == "agent-1" && status == "success"
                    && output.as_deref() == Some("done") && error.is_none()
        ));

        let compacting = EventEnvelope {
            event_type: EventType::ContextCompactionStarted,
            payload: serde_json::json!({}),
            ..base.clone()
        };
        assert!(matches!(
            runtime_event_to_tui(&compacting),
            Some(TuiEvent::CompactionStarted)
        ));

        let compacted = EventEnvelope {
            event_type: EventType::ContextCompacted,
            payload: serde_json::json!({
                "before_messages": 12,
                "after_messages": 4,
                "reason": "pressure",
                "strategy": "summary",
                "collapsed_messages": 8,
                "status_text": "context compacted"
            }),
            ..base
        };
        assert!(matches!(
            runtime_event_to_tui(&compacted),
            Some(TuiEvent::Compacted {
                before_messages: 12,
                after_messages: 4,
                reason,
                strategy,
                collapsed_messages: 8,
                status_text,
            }) if reason == "pressure" && strategy == "summary" && status_text == "context compacted"
        ));
    }

    #[test]
    fn hosted_session_startup_accepts_latest_and_uuid_selectors_only() {
        let preloaded = Arc::new(Mutex::new(None));
        assert!(typed_history_startup_eligible(
            &HistoryMode::Resume("latest".to_string()),
            &preloaded,
        ));
        assert!(typed_history_startup_eligible(
            &HistoryMode::Resume("123e4567-e89b-12d3-a456-426614174000".to_string()),
            &preloaded,
        ));
        assert!(!typed_history_startup_eligible(
            &HistoryMode::Resume("named-session".to_string()),
            &preloaded,
        ));
    }

    #[test]
    fn projected_history_is_enriched_with_persisted_image_messages() {
        let projected = vec![
            ChatMessage::User("inspect [Image #1]".to_string()),
            ChatMessage::Assistant("done".to_string()),
        ];
        let history = vec![
            Message::user_with_images(
                "inspect [Image #1]".to_string(),
                vec![orca_core::conversation::ImageInput {
                    source: orca_core::conversation::ImageSource::Url {
                        url: "https://example.com/image.png".to_string(),
                    },
                    detail: orca_core::conversation::ImageDetail::High,
                }],
            ),
            Message::Assistant {
                content: Some("done".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            },
        ];

        let enriched = attach_history_images(projected, &history);
        assert!(matches!(
            enriched.as_slice(),
            [
                ChatMessage::User(_),
                ChatMessage::Image(image),
                ChatMessage::Assistant(_)
            ] if image.label == "[Image #1]"
        ));
    }

    #[test]
    fn hosted_session_empty_history_emits_the_existing_payload() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        emit_empty_history_snapshot(&event_tx, "Unable to restore saved conversation.");

        assert!(matches!(
            event_rx.try_recv(),
            Ok(crate::types::TuiEvent::HistoryLoaded {
                messages,
                plan: None,
                label,
            }) if messages.is_empty() && label == "Unable to restore saved conversation."
        ));
    }

    #[test]
    fn hosted_session_attached_projection_resets_before_history() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let attachment = SessionAttachmentId::new(7);
        let projection = SurfaceProjectionState {
            cursor: crate::surface_projection::test_surface_cursor(1),
            session_id: Some("session".to_string()),
            title: "Session".to_string(),
            usage_revision: 0,
            usage: orca_core::cost_types::UsageTotals::default(),
            context_revision: 0,
            context_used_tokens: 0,
            context_limit_tokens: 0,
            workflow_tasks: Vec::new(),
            current_goal: None,
            foreground_operation_id: None,
            recoverable_operation_id: None,
            goal_presentation: None,
            session_presentation: None,
        };

        project_hosted_thread_attached(
            projection,
            vec![ChatMessage::User("hello".to_string())],
            attachment,
            &event_tx,
        )
        .unwrap();

        let first = event_rx.recv().unwrap();
        let second = event_rx.recv().unwrap();
        assert!(matches!(
            first,
            TuiEvent::Attached(attached)
                if attached.attachment == Some(attachment)
                    && matches!(attached.event, TuiEvent::SessionProjectionReset(_))
        ));
        assert!(matches!(
            second,
            TuiEvent::Attached(attached)
                if attached.attachment == Some(attachment)
                    && matches!(attached.event, TuiEvent::HistoryLoaded { .. })
        ));
    }
}
