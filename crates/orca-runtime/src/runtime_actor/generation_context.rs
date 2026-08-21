use std::collections::HashMap;
use std::io;

use orca_core::provider_types::ProviderStep;
use orca_core::thread_item_projection::{CompletedModelItem, ModelResponseIdentity};

use crate::model_response::RuntimeModelResponse;
use crate::runtime_host::{surface_persisted_display_text, surface_sha256, surface_tool_action};
use crate::runtime_surface as surface;

struct PendingStreamRedaction {
    fence: surface::SurfaceOperationFence,
    raw_tail: String,
}

pub(crate) struct ProviderStepProjection {
    pub(crate) events: Vec<(surface::SurfaceScope, surface::SurfaceEvent)>,
    pub(crate) background_fence: Option<surface::SurfaceBackgroundFence>,
}

#[derive(Default)]
pub(crate) struct GenerationContextController {
    pending_stream_redactions: HashMap<surface::SurfaceItemId, PendingStreamRedaction>,
}

impl GenerationContextController {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Projects one provider stream delta against a frozen surface snapshot.
    ///
    /// The projection validates the exact generation/turn identity, preserves
    /// sensitive-token redaction tails, and returns ordered events without
    /// committing them.
    pub(crate) fn provider_step_events(
        &mut self,
        snapshot: &surface::SurfaceSnapshot,
        active_generation: bool,
        fence: surface::SurfaceOperationFence,
        identity: &ModelResponseIdentity,
        step: &ProviderStep,
    ) -> io::Result<Option<ProviderStepProjection>> {
        let (channel, item_id, text) = match step {
            ProviderStep::MessageDelta(text) => (
                surface::AssistantChannel::Message,
                identity.item_ids.conversation_item_id.clone(),
                text,
            ),
            ProviderStep::ReasoningDelta(text) => (
                surface::AssistantChannel::Reasoning,
                identity.item_ids.reasoning_item_id.clone(),
                text,
            ),
            _ => return Ok(None),
        };
        if text.is_empty() {
            return Ok(None);
        }
        let Some(text) = self.take_stream_redacted_prefix(&fence, &item_id, text)? else {
            return Ok(None);
        };

        let foregrounded_background = snapshot
            .background_operations
            .iter()
            .find(|background| {
                background.fence.operation_fence == fence
                    && background.task_id.as_ref().is_some_and(|task_id| {
                        snapshot.tasks.iter().any(|task| {
                            &task.task_id == task_id
                                && task.status == surface::SurfaceTaskStatus::Running
                                && !task.backgrounded
                                && task.background_fence.is_none()
                        })
                    })
            })
            .map(|background| background.fence.clone());
        if !active_generation && foregrounded_background.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider step generation fence is stale or not foreground-attached",
            ));
        }
        let operation = surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        if generation.logical_turn_id != identity.turn_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider step turn identity differs from active generation",
            ));
        }

        let scope = foregrounded_background
            .as_ref()
            .map(|fence| surface::SurfaceScope::Background {
                fence: fence.clone(),
            })
            .unwrap_or_else(|| surface::SurfaceScope::Generation {
                fence: fence.clone(),
            });
        let mut events = Vec::with_capacity(2);
        let (stream_id, offset) = if let Some(stream) = snapshot
            .assistant_streams
            .iter()
            .find(|stream| stream.item_id == item_id && stream.channel == channel)
        {
            if stream.fence != fence
                || stream.turn_id != identity.turn_id
                || stream.state != surface::SurfaceAssistantStreamState::Open
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider step targets a closed or foreign assistant stream",
                ));
            }
            (stream.stream_id.clone(), stream.next_offset)
        } else {
            let raw_id = item_id
                .as_str()
                .strip_prefix("item_")
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "provider step item identity is not UUIDv7-backed",
                    )
                })?;
            let stream_id =
                surface::SurfaceStreamId::try_from_bytes(*raw_id.as_bytes()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "provider step item identity is not UUIDv7-backed",
                    )
                })?;
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamOpened {
                    stream: surface::SurfaceAssistantStream {
                        stream_id: stream_id.clone(),
                        fence: fence.clone(),
                        turn_id: identity.turn_id.clone(),
                        item_id,
                        channel,
                        next_offset: surface::ByteOffset::new(0),
                        text: surface::DisplayText::new(""),
                        state: surface::SurfaceAssistantStreamState::Open,
                    },
                }),
            ));
            (stream_id, surface::ByteOffset::new(0))
        };
        events.push((
            scope,
            surface::SurfaceEvent::Assistant(surface::AssistantPatch::Delta {
                stream_id,
                offset,
                text,
            }),
        ));
        Ok(Some(ProviderStepProjection {
            events,
            background_fence: foregrounded_background,
        }))
    }

    /// Builds idempotent foreground provider completion events from a frozen snapshot.
    pub(crate) fn provider_response_events(
        &mut self,
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        response: &RuntimeModelResponse,
    ) -> io::Result<Vec<(surface::SurfaceScope, surface::SurfaceEvent)>> {
        let normalized = normalize_provider_response(response, "provider response")?;
        self.clear_item_ids(normalized.response_item_ids());
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };

        let response_items_match = response_items_match(snapshot, &normalized.completed_response);
        let has_response_identity = response_items_match.iter().any(Option::is_some)
            || !normalized.tool_requests.is_empty();
        let tools_match = tools_match(snapshot, &normalized);
        if has_response_identity
            && response_items_match
                .into_iter()
                .flatten()
                .all(|matched| matched)
            && tools_match
        {
            return Ok(context_usage_events(
                snapshot,
                scope,
                response.response.usage,
            ));
        }
        Ok(completion_events(
            snapshot,
            fence,
            scope,
            normalized,
            response.response.usage,
        ))
    }

    /// Builds idempotent background provider completion events from a frozen snapshot.
    pub(crate) fn background_provider_response_events(
        &mut self,
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceBackgroundFence,
        response: &RuntimeModelResponse,
    ) -> io::Result<Vec<(surface::SurfaceScope, surface::SurfaceEvent)>> {
        self.clear_response_items(response);
        build_background_provider_response_events(snapshot, fence, response)
    }

    pub(crate) fn provider_attempt_failure_events(
        &mut self,
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        identity: &ModelResponseIdentity,
    ) -> io::Result<Vec<(surface::SurfaceScope, surface::SurfaceEvent)>> {
        let operation = surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        if generation.logical_turn_id != identity.turn_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider attempt turn identity differs from active generation",
            ));
        }
        let item_ids = [
            &identity.item_ids.conversation_item_id,
            &identity.item_ids.reasoning_item_id,
            &identity.item_ids.plan_item_id,
        ];
        self.clear_item_ids(item_ids.iter().copied());
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        Ok(snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == *fence
                    && stream.turn_id == identity.turn_id
                    && stream.state == surface::SurfaceAssistantStreamState::Open
                    && item_ids.iter().any(|item_id| **item_id == stream.item_id)
            })
            .map(|stream| {
                (
                    scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: surface::AssistantDiscardReason::ProviderFailed,
                    }),
                )
            })
            .collect())
    }

    pub(crate) fn clear_operation(&mut self, operation_id: &surface::SurfaceOperationId) {
        self.pending_stream_redactions
            .retain(|_, pending| &pending.fence.operation_id != operation_id);
    }

    pub(crate) fn clear_response_items(&mut self, response: &RuntimeModelResponse) {
        self.clear_item_ids(
            response
                .completed()
                .completed_items()
                .iter()
                .map(CompletedModelItem::id),
        );
    }

    fn clear_item_ids<'a>(
        &mut self,
        item_ids: impl IntoIterator<Item = &'a surface::SurfaceItemId>,
    ) {
        for item_id in item_ids {
            self.pending_stream_redactions.remove(item_id);
        }
    }

    fn take_stream_redacted_prefix(
        &mut self,
        fence: &surface::SurfaceOperationFence,
        item_id: &surface::SurfaceItemId,
        text: &str,
    ) -> io::Result<Option<surface::DisplayText>> {
        let pending = self
            .pending_stream_redactions
            .entry(item_id.clone())
            .or_insert_with(|| PendingStreamRedaction {
                fence: fence.clone(),
                raw_tail: String::new(),
            });
        if pending.fence != *fence {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider stream redaction tail belongs to another generation",
            ));
        }
        pending.raw_tail.push_str(text);
        let stable_len = stable_surface_stream_prefix_len(&pending.raw_tail);
        if stable_len == 0 {
            return Ok(None);
        }
        let stable = pending.raw_tail.drain(..stable_len).collect::<String>();
        Ok(Some(surface_persisted_display_text(&stable)))
    }
}

pub(crate) fn build_background_provider_response_events(
    snapshot: &surface::SurfaceSnapshot,
    fence: &surface::SurfaceBackgroundFence,
    response: &RuntimeModelResponse,
) -> io::Result<Vec<(surface::SurfaceScope, surface::SurfaceEvent)>> {
    validated_response_id(response, "background provider response")?;
    let completed = response.completed();
    let response_item_ids = completed.completed_items();
    let scope = surface::SurfaceScope::Background {
        fence: fence.clone(),
    };
    if snapshot.items.iter().any(|item| {
        surface_item_id(item).is_some_and(|id| {
            response_item_ids
                .iter()
                .any(|response_item| response_item.id() == id)
        })
    }) {
        return Ok(context_usage_events(
            snapshot,
            scope,
            response.response.usage,
        ));
    }
    let normalized = normalize_provider_response(response, "background provider response")?;
    Ok(completion_events(
        snapshot,
        &fence.operation_fence,
        scope,
        normalized,
        response.response.usage,
    ))
}

struct NormalizedProviderResponse {
    completed_response: surface::SurfaceCompletedModelResponse,
    tool_requests: Vec<surface::SurfaceToolRequest>,
}

impl NormalizedProviderResponse {
    fn response_item_ids(&self) -> impl Iterator<Item = &surface::SurfaceItemId> {
        [
            self.completed_response
                .message_item
                .as_ref()
                .map(|item| &item.id),
            self.completed_response
                .reasoning_item
                .as_ref()
                .map(|item| &item.id),
            self.completed_response
                .plan_item
                .as_ref()
                .map(|item| &item.id),
        ]
        .into_iter()
        .flatten()
    }
}

fn normalize_provider_response(
    response: &RuntimeModelResponse,
    label: &str,
) -> io::Result<NormalizedProviderResponse> {
    let completed = response.completed();
    let response_id = validated_response_id(response, label)?;
    let mut message_item = None;
    let mut reasoning_item = None;
    let mut plan_item = None;
    for item in completed.completed_items() {
        match item {
            CompletedModelItem::AgentMessage { id, text } => {
                message_item = Some(surface::SurfaceAssistantMessageItem {
                    id,
                    turn_id: completed.identity.turn_id.clone(),
                    text: surface_persisted_display_text(&text),
                    pinned: false,
                });
            }
            CompletedModelItem::Reasoning {
                id,
                summary,
                content,
            } => {
                let (summary, content) = if content.is_empty() && !summary.is_empty() {
                    (String::new(), summary)
                } else {
                    (summary, content)
                };
                reasoning_item = Some(surface::SurfaceAssistantReasoningItem {
                    id,
                    turn_id: completed.identity.turn_id.clone(),
                    summary: surface_persisted_display_text(&summary),
                    content: surface_persisted_display_text(&content),
                    pinned: false,
                });
            }
            CompletedModelItem::Plan { id, text } => {
                plan_item = Some(surface::SurfaceAssistantPlanItem {
                    id,
                    turn_id: completed.identity.turn_id.clone(),
                    text: surface_persisted_display_text(&text),
                    pinned: false,
                });
            }
        }
    }

    let mut requests_by_id = HashMap::new();
    for step in &response.response.steps {
        if let ProviderStep::ToolCall(request) = step
            && requests_by_id
                .insert(request.id.clone(), request.clone())
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} repeats a tool call id"),
            ));
        }
    }
    if requests_by_id.len() != completed.tool_calls.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} tool metadata is incomplete"),
        ));
    }

    let mut raw_tool_calls = Vec::with_capacity(completed.tool_calls.len());
    let mut tool_requests = Vec::with_capacity(completed.tool_calls.len());
    for raw_call in &completed.tool_calls {
        let request = requests_by_id.get(&raw_call.id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} lacks the executable tool request"),
            )
        })?;
        let raw_arguments = request.raw_arguments.clone().unwrap_or_default();
        if request.name.as_str() != raw_call.function_name || raw_arguments != raw_call.arguments {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} tool identity differs from executable request"),
            ));
        }
        let tool_call_id = surface::SurfaceToolCallId::try_new(raw_call.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
        let name = surface::NonEmptyText::try_new(raw_call.function_name.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool name"))?;
        let arguments_digest = surface_sha256(raw_call.arguments.as_bytes());
        raw_tool_calls.push(surface::SurfaceRawToolCall {
            id: tool_call_id.clone(),
            name: name.clone(),
            raw_arguments: surface::DisplayText::new(raw_call.arguments.clone()),
            arguments_digest,
        });
        tool_requests.push(surface::SurfaceToolRequest {
            tool_call_id,
            source_response_id: Some(response_id.clone()),
            turn_id: completed.identity.turn_id.clone(),
            name,
            action: surface_tool_action(request.action),
            target: request.target.clone().map(surface::DisplayText::new),
            raw_arguments: surface::DisplayText::new(raw_call.arguments.clone()),
            arguments_digest,
        });
    }

    Ok(NormalizedProviderResponse {
        completed_response: surface::SurfaceCompletedModelResponse {
            response_id,
            turn_id: completed.identity.turn_id.clone(),
            message_item,
            reasoning_item,
            plan_item,
            tool_calls: raw_tool_calls,
        },
        tool_requests,
    })
}

fn validated_response_id(
    response: &RuntimeModelResponse,
    label: &str,
) -> io::Result<surface::UuidV7> {
    let completed = response.completed();
    let response_uuid = completed
        .identity
        .item_ids
        .conversation_item_id
        .as_str()
        .strip_prefix("item_")
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} identity is not UUIDv7-backed"),
            )
        })?;
    let response_id = surface::UuidV7::try_from_bytes(*response_uuid.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} identity is not UUIDv7-backed"),
        )
    })?;
    Ok(response_id)
}

fn completion_events(
    snapshot: &surface::SurfaceSnapshot,
    operation_fence: &surface::SurfaceOperationFence,
    scope: surface::SurfaceScope,
    normalized: NormalizedProviderResponse,
    usage: Option<orca_core::provider_types::Usage>,
) -> Vec<(surface::SurfaceScope, surface::SurfaceEvent)> {
    let mut events = Vec::new();
    for stream in snapshot.assistant_streams.iter().filter(|stream| {
        stream.fence == *operation_fence
            && stream.turn_id == normalized.completed_response.turn_id
            && stream.state == surface::SurfaceAssistantStreamState::Open
    }) {
        let completed_text = match stream.channel {
            surface::AssistantChannel::Message => normalized
                .completed_response
                .message_item
                .as_ref()
                .filter(|item| item.id == stream.item_id)
                .map(|item| item.text.as_str()),
            surface::AssistantChannel::Reasoning => normalized
                .completed_response
                .reasoning_item
                .as_ref()
                .filter(|item| item.id == stream.item_id)
                .map(|item| item.content.as_str()),
            surface::AssistantChannel::Plan => normalized
                .completed_response
                .plan_item
                .as_ref()
                .filter(|item| item.id == stream.item_id)
                .map(|item| item.text.as_str()),
        };
        match completed_text.and_then(|text| text.strip_prefix(stream.text.as_str())) {
            Some(suffix) if !suffix.is_empty() => events.push((
                scope.clone(),
                surface::SurfaceEvent::Assistant(surface::AssistantPatch::Delta {
                    stream_id: stream.stream_id.clone(),
                    offset: stream.next_offset,
                    text: surface::DisplayText::new(suffix),
                }),
            )),
            Some(_) => {}
            None => events.push((
                scope.clone(),
                surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                    stream_id: stream.stream_id.clone(),
                    reason: surface::AssistantDiscardReason::ProviderFailed,
                }),
            )),
        }
    }
    events.push((
        scope.clone(),
        surface::SurfaceEvent::Assistant(surface::AssistantPatch::ResponseCompleted {
            response: normalized.completed_response,
        }),
    ));
    events.extend(normalized.tool_requests.into_iter().map(|request| {
        (
            scope.clone(),
            surface::SurfaceEvent::Tool(surface::ToolPatch::Requested { request }),
        )
    }));
    if let Some(context) =
        usage.and_then(|usage| next_provider_context_snapshot(&snapshot.context, usage))
    {
        events.push((scope, surface::SurfaceEvent::Context(context)));
    }
    events
}

fn context_usage_events(
    snapshot: &surface::SurfaceSnapshot,
    scope: surface::SurfaceScope,
    usage: Option<orca_core::provider_types::Usage>,
) -> Vec<(surface::SurfaceScope, surface::SurfaceEvent)> {
    usage
        .and_then(|usage| next_provider_context_snapshot(&snapshot.context, usage))
        .map(|context| vec![(scope, surface::SurfaceEvent::Context(context))])
        .unwrap_or_default()
}

fn response_items_match(
    snapshot: &surface::SurfaceSnapshot,
    completed: &surface::SurfaceCompletedModelResponse,
) -> [Option<bool>; 3] {
    [
        completed.message_item.as_ref().map(|expected| {
            snapshot.items.iter().any(|item| {
                matches!(
                    item,
                    surface::SurfaceItem::AssistantMessage { id, turn_id, text, pinned }
                        if id == &expected.id
                            && turn_id == &expected.turn_id
                            && text == &expected.text
                            && pinned == &expected.pinned
                )
            })
        }),
        completed.reasoning_item.as_ref().map(|expected| {
            snapshot.items.iter().any(|item| {
                matches!(
                    item,
                    surface::SurfaceItem::AssistantReasoning { id, turn_id, summary, content, pinned }
                        if id == &expected.id
                            && turn_id == &expected.turn_id
                            && summary == &expected.summary
                            && content == &expected.content
                            && pinned == &expected.pinned
                )
            })
        }),
        completed.plan_item.as_ref().map(|expected| {
            snapshot.items.iter().any(|item| {
                matches!(
                    item,
                    surface::SurfaceItem::AssistantPlan { id, turn_id, text, pinned }
                        if id == &expected.id
                            && turn_id == &expected.turn_id
                            && text == &expected.text
                            && pinned == &expected.pinned
                )
            })
        }),
    ]
}

fn tools_match(
    snapshot: &surface::SurfaceSnapshot,
    normalized: &NormalizedProviderResponse,
) -> bool {
    snapshot
        .tools
        .iter()
        .filter(|tool| {
            tool.request.source_response_id.as_ref()
                == Some(&normalized.completed_response.response_id)
                && tool.request.turn_id == normalized.completed_response.turn_id
        })
        .count()
        == normalized.tool_requests.len()
        && normalized
            .tool_requests
            .iter()
            .all(|expected| snapshot.tools.iter().any(|tool| tool.request == *expected))
}

fn surface_item_id(item: &surface::SurfaceItem) -> Option<&surface::SurfaceItemId> {
    match item {
        surface::SurfaceItem::AssistantMessage { id, .. }
        | surface::SurfaceItem::AssistantReasoning { id, .. }
        | surface::SurfaceItem::AssistantPlan { id, .. } => Some(id),
        _ => None,
    }
}

fn surface_operation_record<'a>(
    snapshot: &'a surface::SurfaceSnapshot,
    operation_id: &surface::SurfaceOperationId,
) -> Option<&'a surface::OperationRecord> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| &operation.operation_id == operation_id)
}

pub(crate) fn next_provider_context_snapshot(
    current: &surface::SurfaceContextSnapshot,
    usage: orca_core::provider_types::Usage,
) -> Option<surface::SurfaceContextSnapshot> {
    if usage.is_empty() || current.limit_tokens == 0 {
        return None;
    }
    let used_tokens = usage.input_tokens.min(current.limit_tokens);
    if current.used_tokens == used_tokens {
        return None;
    }
    let revision =
        surface::ContextRevision::try_new(current.revision.get().checked_add(1)?).ok()?;
    let mut next = current.clone();
    next.revision = revision;
    next.used_tokens = used_tokens;
    Some(next)
}

pub(crate) fn stable_surface_stream_prefix_len(text: &str) -> usize {
    if let Some(sensitive_assignment_start) = incomplete_surface_sensitive_assignment_start(text) {
        return sensitive_assignment_start;
    }
    let trailing_token_start = text
        .char_indices()
        .filter_map(|(index, ch)| {
            (ch.is_whitespace()
                || matches!(
                    ch,
                    '"' | '\'' | '{' | '}' | '[' | ']' | ',' | ':' | ';' | '(' | ')'
                ))
            .then_some(index + ch.len_utf8())
        })
        .next_back()
        .unwrap_or(0);
    let trailing_token = &text[trailing_token_start..];
    let lower = trailing_token.to_ascii_lowercase();
    let mut retained_start = None;
    for needle in [
        "api_key",
        "apikey",
        "token",
        "password",
        "secret",
        "authorization",
    ] {
        if let Some(index) = lower.find(needle) {
            retained_start =
                Some(retained_start.map_or(index, |current: usize| current.min(index)));
            continue;
        }
        for prefix_len in 1..needle.len() {
            if lower.ends_with(&needle[..prefix_len]) {
                let index = lower.len() - prefix_len;
                retained_start =
                    Some(retained_start.map_or(index, |current: usize| current.min(index)));
            }
        }
    }
    if matches!(lower.as_str(), "s" | "sk") || lower.starts_with("sk-") {
        retained_start = Some(0);
    }
    retained_start
        .map(|index| trailing_token_start + index)
        .unwrap_or(text.len())
}

fn incomplete_surface_sensitive_assignment_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'=' && bytes[index] != b':' {
            index += 1;
            continue;
        }
        let key_start = surface_sensitive_key_start(bytes, index);
        let key = text[key_start..index].trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '[' | ',')
        });
        if !is_surface_sensitive_key(key) {
            index += 1;
            continue;
        }
        let mut value_start = index + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        if value_start == bytes.len() {
            return Some(key_start);
        }
        if matches!(bytes[value_start], b'"' | b'\'') {
            let quote = bytes[value_start];
            let mut value_index = value_start + 1;
            let mut escaped = false;
            let mut closed = false;
            while value_index < bytes.len() {
                let byte = bytes[value_index];
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    closed = true;
                    value_index += 1;
                    break;
                }
                value_index += 1;
            }
            if !closed {
                return Some(key_start);
            }
            index = value_index;
            continue;
        }
        let mut value_index = value_start;
        while value_index < bytes.len()
            && !bytes[value_index].is_ascii_whitespace()
            && !matches!(bytes[value_index], b',' | b'}' | b']' | b';')
        {
            value_index += 1;
        }
        if value_index == bytes.len() {
            return Some(key_start);
        }
        index = value_index + 1;
    }
    None
}

fn surface_sensitive_key_start(bytes: &[u8], delimiter_index: usize) -> usize {
    let mut start = delimiter_index;
    while start > 0 {
        let previous = bytes[start - 1];
        if previous.is_ascii_whitespace() || matches!(previous, b'{' | b'[' | b',' | b';' | b'(') {
            break;
        }
        start -= 1;
    }
    start
}

fn is_surface_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("authorization")
}

#[cfg(test)]
mod tests {
    use super::next_provider_context_snapshot;
    use crate::runtime_surface as surface;

    #[test]
    fn provider_usage_updates_context_snapshot_with_current_prompt_tokens_bits_spec_ut() {
        let current = surface::SurfaceContextSnapshot {
            revision: surface::ContextRevision::try_new(7).unwrap(),
            used_tokens: 42,
            limit_tokens: 1_000_000,
            compaction: surface::CompactionState::Idle,
            fragments: Vec::new(),
            provider_replay: surface::ProviderReplayHealth::None,
        };
        let next = next_provider_context_snapshot(
            &current,
            orca_core::provider_types::Usage {
                input_tokens: 151_063,
                output_tokens: 12_345,
                cache_tokens: 140_000,
            },
        )
        .unwrap();
        assert_eq!(next.revision.get(), 8);
        assert_eq!(next.used_tokens, 151_063);
        assert_eq!(next.limit_tokens, 1_000_000);
    }

    #[test]
    fn provider_usage_is_bounded_by_context_limit_bits_spec_ut() {
        let current = surface::SurfaceContextSnapshot {
            revision: surface::ContextRevision::try_new(1).unwrap(),
            used_tokens: 0,
            limit_tokens: 100,
            compaction: surface::CompactionState::Idle,
            fragments: Vec::new(),
            provider_replay: surface::ProviderReplayHealth::None,
        };
        let next = next_provider_context_snapshot(
            &current,
            orca_core::provider_types::Usage {
                input_tokens: 101,
                output_tokens: 0,
                cache_tokens: 0,
            },
        )
        .unwrap();
        assert_eq!(next.used_tokens, 100);
    }
}
