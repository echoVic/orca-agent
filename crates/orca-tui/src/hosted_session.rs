//! Stateless hosted session snapshot and history event shaping.

use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;
use orca_core::config::HistoryMode;
use orca_core::conversation::Message;
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_runtime::history;
use orca_runtime::runtime_host::RuntimeThreadHandle;
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::attachment_routing::send_attached_event;
use crate::composer_images::ComposerImageState;
use crate::surface_actions::TuiSurfaceActions;
use crate::surface_projection::{SessionProjectionPresentation, SurfaceProjectionState};
use crate::types::{ChatMessage, SessionAttachmentId, TuiEvent};

fn start_prompt_queue_watcher(
    mut queue_updates: tokio::sync::watch::Receiver<
        orca_runtime::prompt_queue::PromptQueueSnapshot,
    >,
    event_tx: &mpsc::Sender<TuiEvent>,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) {
    let queue_events = event_tx.clone();
    let watcher = Box::new(move || {
        loop {
            match queue_updates.has_changed() {
                Ok(true) => {
                    let snapshot = queue_updates.borrow_and_update().clone();
                    if queue_events
                        .send(TuiEvent::PromptQueueUpdated(snapshot))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(false) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    });
    if let Err(error) = spawn(watcher) {
        let _ = event_tx.send(TuiEvent::Error(format!(
            "failed to start prompt queue watcher: {error}"
        )));
    }
}

pub(crate) fn announce_runtime_ready(
    thread: &RuntimeThreadHandle,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let _ = event_tx.send(TuiEvent::MentionRuntimeReady(thread.typed_surface()));
    if let Ok(snapshot) = thread.prompt_queue(orca_runtime::prompt_queue::PromptQueueAction::List) {
        let _ = event_tx.send(TuiEvent::PromptQueueUpdated(snapshot));
    }
    start_prompt_queue_watcher(thread.subscribe_prompt_queue(), event_tx, |watcher| {
        std::thread::Builder::new()
            .name("orca-tui-prompt-queue".to_string())
            .spawn(watcher)
            .map(|_| ())
    });
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

    use super::*;

    #[test]
    fn prompt_queue_watcher_spawn_failure_emits_error() {
        let (_queue_tx, queue_updates) =
            tokio::sync::watch::channel(orca_runtime::prompt_queue::PromptQueueSnapshot::default());
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        start_prompt_queue_watcher(queue_updates, &event_tx, |_| {
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
