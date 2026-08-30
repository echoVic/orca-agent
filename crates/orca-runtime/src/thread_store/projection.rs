use std::collections::{HashMap, HashSet};
use std::io;

use orca_core::conversation::{
    Message, RawToolCall, assistant_message_has_payload, normalize_tool_boundaries,
    repaired_missing_tool_result,
};
use orca_core::thread_item_projection::ProjectedToolTerminalMetadata;
use orca_core::tool_types::ToolTerminal;
use serde_json::{Value, json};

use crate::tool_item_projection::{
    ProjectedPersistedMessageThreadItem, complete_projected_tool_item, dynamic_tool_started_item,
    mcp_tool_parts, mcp_tool_started_item, parse_json_or_null,
    persisted_command_execution_started_item, persisted_file_change_started_item,
    persisted_message_thread_item,
};

use super::types::{
    SessionRecord, SessionSummary, StoredConversationRecord, StoredMessage, StoredThreadItem,
    StoredThreadSummary, StoredThreadTurn, TurnItemsView,
};

pub(crate) fn is_surface_coordinator_record(record: &SessionRecord) -> bool {
    matches!(
        record,
        SessionRecord::SurfaceCommitPrepared { .. }
            | SessionRecord::SurfaceCommitCommitted { .. }
            | SessionRecord::SurfaceOwnerEpoch { .. }
            | SessionRecord::SurfaceFinalizeIntent { .. }
            | SessionRecord::SurfaceSettlement { .. }
            | SessionRecord::SurfaceShutdownBarrier { .. }
    )
}

pub(crate) fn message_to_thread_json(message: &Message) -> Value {
    match message {
        Message::System { content, .. } => {
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::system(content))
        }
        Message::User { content, .. } => {
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::user(content))
        }
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => persisted_message_thread_item(ProjectedPersistedMessageThreadItem::assistant(
            content.clone(),
            reasoning_content.clone(),
            tool_calls_to_values(tool_calls),
        )),
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => persisted_message_thread_item(ProjectedPersistedMessageThreadItem::tool(
            tool_call_id,
            content,
        )),
    }
}

pub(crate) fn stored_message_to_thread_json(message: &StoredMessage) -> Value {
    match message {
        StoredMessage::System { content, .. } => {
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::system(content))
        }
        StoredMessage::User { content, .. } => {
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::user(content))
        }
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => persisted_message_thread_item(ProjectedPersistedMessageThreadItem::assistant(
            content.clone(),
            reasoning_content.clone(),
            tool_calls_to_values(tool_calls),
        )),
        StoredMessage::Tool {
            tool_call_id,
            content,
            ..
        } => persisted_message_thread_item(ProjectedPersistedMessageThreadItem::tool(
            tool_call_id,
            content,
        )),
    }
}

fn tool_calls_to_values(tool_calls: &[RawToolCall]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|tool_call| serde_json::to_value(tool_call).expect("raw tool call serializes"))
        .collect()
}

struct ProjectedConversationItem {
    item_id: Option<String>,
    item: Value,
}

struct ProjectedConversationTurn {
    turn_id: String,
    index: usize,
    role: String,
    identified: bool,
    items: Vec<ProjectedConversationItem>,
}

pub(crate) fn conversation_records_to_thread_turns(
    thread_id: &str,
    records: &[StoredConversationRecord],
    limit: usize,
    items_view: TurnItemsView,
) -> io::Result<Vec<StoredThreadTurn>> {
    Ok(project_conversation_records(records)?
        .into_iter()
        .take(limit)
        .map(|turn| StoredThreadTurn {
            thread_id: thread_id.to_string(),
            turn_id: turn.turn_id,
            index: turn.index,
            role: turn.role,
            items_view,
            items: if items_view == TurnItemsView::NotLoaded {
                Vec::new()
            } else {
                turn.items.into_iter().map(|item| item.item).collect()
            },
        })
        .collect())
}

pub(crate) fn conversation_records_to_thread_items(
    thread_id: &str,
    records: &[StoredConversationRecord],
    turn_id: Option<&str>,
    limit: usize,
) -> io::Result<Vec<StoredThreadItem>> {
    Ok(project_conversation_records(records)?
        .into_iter()
        .flat_map(|turn| {
            turn.items
                .into_iter()
                .map(move |item| (turn.turn_id.clone(), item))
        })
        .enumerate()
        .map(|(index, (item_turn_id, item))| StoredThreadItem {
            thread_id: thread_id.to_string(),
            turn_id: item_turn_id,
            item_id: item.item_id.unwrap_or_else(|| item_id_for_index(index)),
            index,
            item: item.item,
        })
        .filter(|item| turn_id.is_none_or(|requested| requested == item.turn_id))
        .take(limit)
        .collect())
}

pub(crate) fn normalized_stored_messages(
    records: &[StoredConversationRecord],
) -> Vec<StoredMessage> {
    let mut messages = records
        .iter()
        .map(|record| Message::from(record.message.clone()))
        .collect::<Vec<_>>();
    normalize_tool_boundaries(&mut messages);
    messages.iter().map(StoredMessage::from).collect()
}

fn project_conversation_records(
    records: &[StoredConversationRecord],
) -> io::Result<Vec<ProjectedConversationTurn>> {
    let mut turns = Vec::new();
    let mut closed_identified_turns = HashSet::new();
    let mut index = 0usize;

    while index < records.len() {
        let record = &records[index];
        validate_record_identity(record)?;
        match &record.message {
            StoredMessage::System { .. } | StoredMessage::Tool { .. } => {
                index += 1;
            }
            StoredMessage::Assistant {
                content,
                tool_calls,
                ..
            } if !assistant_message_has_payload(content.as_deref(), tool_calls) => {
                index += 1;
            }
            StoredMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } if !tool_calls.is_empty() => {
                let mut seen_call_ids = HashSet::new();
                let unique_tool_calls = tool_calls
                    .iter()
                    .filter(|tool_call| seen_call_ids.insert(tool_call.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                let assistant = StoredMessage::Assistant {
                    content: content.clone(),
                    reasoning_content: reasoning_content.clone(),
                    tool_calls: unique_tool_calls.clone(),
                    pinned: *pinned,
                };
                let mut items = projected_record_items(record, &assistant)?;
                let expected = unique_tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.as_str())
                    .collect::<HashSet<_>>();
                let mut results = HashMap::new();
                let mut next = index + 1;
                while next < records.len() {
                    let tool_record = &records[next];
                    let StoredMessage::Tool {
                        tool_call_id,
                        content,
                        terminal,
                        ..
                    } = &tool_record.message
                    else {
                        break;
                    };
                    validate_record_identity(tool_record)?;
                    validate_tool_result_owner(record, tool_record)?;
                    if expected.contains(tool_call_id.as_str()) {
                        results.entry(tool_call_id.clone()).or_insert_with(|| {
                            tool_result_to_thread_item(
                                tool_call_id,
                                content,
                                terminal.terminal_ref(),
                            )
                        });
                    }
                    next += 1;
                }
                for tool_call in &unique_tool_calls {
                    let result = results
                        .remove(&tool_call.id)
                        .unwrap_or_else(|| repaired_tool_result_item(tool_call));
                    merge_projected_record_items(
                        &mut items,
                        vec![ProjectedConversationItem {
                            item_id: None,
                            item: result,
                        }],
                    );
                }
                push_projected_record(
                    &mut turns,
                    &mut closed_identified_turns,
                    record,
                    "assistant",
                    items,
                )?;
                index = next.max(index + 1);
            }
            message => {
                let role = stored_message_role(message);
                let items = projected_record_items(record, message)?;
                push_projected_record(
                    &mut turns,
                    &mut closed_identified_turns,
                    record,
                    role,
                    items,
                )?;
                index += 1;
            }
        }
    }

    Ok(turns)
}

fn validate_record_identity(record: &StoredConversationRecord) -> io::Result<()> {
    if record.item_id.is_some() == record.turn_id.is_some() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "conversation record id and turn_id must be present together",
    ))
}

fn validate_tool_result_owner(
    request: &StoredConversationRecord,
    result: &StoredConversationRecord,
) -> io::Result<()> {
    if request.turn_id == result.turn_id && request.item_id.is_some() == result.item_id.is_some() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "tool result identity does not match its assistant turn",
    ))
}

fn projected_record_items(
    record: &StoredConversationRecord,
    message: &StoredMessage,
) -> io::Result<Vec<ProjectedConversationItem>> {
    if let Some(completed_items) = &record.completed_model_items {
        let mut projected = completed_items
            .iter()
            .cloned()
            .map(|item| ProjectedConversationItem {
                item_id: Some(item.id().to_string()),
                item: item.into_value(),
            })
            .collect::<Vec<_>>();
        if let StoredMessage::Assistant { tool_calls, .. } = message {
            projected.extend(tool_calls.iter().map(|tool_call| {
                let item = tool_call_to_thread_item(tool_call);
                ProjectedConversationItem {
                    item_id: item["id"].as_str().map(ToString::to_string),
                    item,
                }
            }));
        }
        return Ok(projected);
    }
    let identified = record.item_id.is_some();
    legacy_stored_message_to_thread_items_for_projection(message)
        .into_iter()
        .map(|item| {
            let item_id = if identified {
                if item.get("role").is_some() {
                    record.item_id.as_ref().map(ToString::to_string)
                } else if item["type"] == "tool_result" {
                    None
                } else {
                    Some(
                        item["id"]
                            .as_str()
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "identified tool item is missing its domain id",
                                )
                            })?
                            .to_string(),
                    )
                }
            } else {
                None
            };
            Ok(ProjectedConversationItem { item_id, item })
        })
        .collect()
}

fn push_projected_record(
    turns: &mut Vec<ProjectedConversationTurn>,
    closed_identified_turns: &mut HashSet<String>,
    record: &StoredConversationRecord,
    role: &str,
    items: Vec<ProjectedConversationItem>,
) -> io::Result<()> {
    if let Some(turn_id) = record.turn_id.as_ref() {
        let turn_id = turn_id.to_string();
        if turns.last().is_some_and(|turn| turn.turn_id == turn_id) {
            merge_projected_record_items(&mut turns.last_mut().expect("last turn").items, items);
            return Ok(());
        }
        if !closed_identified_turns.insert(turn_id.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("identified turn {turn_id} is not contiguous"),
            ));
        }
        let index = turns.len();
        turns.push(ProjectedConversationTurn {
            turn_id,
            index,
            role: role.to_string(),
            identified: true,
            items,
        });
        return Ok(());
    }

    let starts_turn = turns.last().is_none_or(|turn| turn.identified) || role == "user";
    if starts_turn {
        let index = turns.len();
        turns.push(ProjectedConversationTurn {
            turn_id: turn_id_for_index(index),
            index,
            role: role.to_string(),
            identified: false,
            items,
        });
    } else if let Some(turn) = turns.last_mut() {
        merge_projected_record_items(&mut turn.items, items);
    }
    Ok(())
}

fn merge_projected_record_items(
    turn_items: &mut Vec<ProjectedConversationItem>,
    items: Vec<ProjectedConversationItem>,
) {
    for item in items {
        if item.item["type"] == "tool_result"
            && let Some(tool_call_id) = item.item["toolCallId"].as_str()
            && let Some(existing) = turn_items
                .iter_mut()
                .rev()
                .find(|candidate| projected_tool_item_matches_result(&candidate.item, tool_call_id))
        {
            complete_tool_item(&mut existing.item, &item.item);
            continue;
        }
        turn_items.push(item);
    }
}

fn repaired_tool_result_item(tool_call: &RawToolCall) -> Value {
    let Message::Tool {
        tool_call_id,
        content,
        terminal,
        ..
    } = repaired_missing_tool_result(tool_call)
    else {
        unreachable!("tool repair always creates a tool result")
    };
    tool_result_to_thread_item(&tool_call_id, &content, terminal.as_ref())
}

pub(crate) fn messages_to_thread_turns(
    thread_id: &str,
    messages: &[Message],
    limit: usize,
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    group_messages_into_thread_turns(thread_id, messages, items_view)
        .into_iter()
        .take(limit)
        .collect()
}

pub(crate) fn messages_to_thread_items(
    thread_id: &str,
    messages: &[Message],
    turn_id: Option<&str>,
    limit: usize,
) -> Vec<StoredThreadItem> {
    group_messages_into_thread_turns(thread_id, messages, TurnItemsView::Full)
        .into_iter()
        .flat_map(|turn| {
            turn.items
                .into_iter()
                .map(move |item| (turn.turn_id.clone(), item))
        })
        .enumerate()
        .map(|(item_index, (item_turn_id, item))| StoredThreadItem {
            thread_id: thread_id.to_string(),
            turn_id: item_turn_id,
            item_id: item_id_for_index(item_index),
            index: item_index,
            item,
        })
        .filter(|item| turn_id.is_none_or(|requested| requested == item.turn_id))
        .take(limit)
        .collect()
}

fn group_messages_into_thread_turns(
    thread_id: &str,
    messages: &[Message],
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    let mut turns = Vec::new();
    for message in messages {
        if matches!(message, Message::System { .. }) {
            continue;
        }
        let items = message_to_thread_items_for_projection(message);
        let role = message_role(message).to_string();
        let starts_turn = turns.is_empty() || matches!(message, Message::User { .. });

        if starts_turn {
            let index = turns.len();
            turns.push(StoredThreadTurn {
                thread_id: thread_id.to_string(),
                turn_id: turn_id_for_index(index),
                index,
                role,
                items_view,
                items: items_for_view(items_view, items),
            });
        } else if let Some(turn) = turns.last_mut() {
            if turn.items_view != TurnItemsView::NotLoaded {
                merge_projected_items(&mut turn.items, items);
            }
        }
    }
    turns
}

fn message_role(message: &Message) -> &'static str {
    match message {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::Tool { .. } => "tool",
    }
}

fn stored_message_role(message: &StoredMessage) -> &'static str {
    match message {
        StoredMessage::System { .. } => "system",
        StoredMessage::User { .. } => "user",
        StoredMessage::Assistant { .. } => "assistant",
        StoredMessage::Tool { .. } => "tool",
    }
}

fn message_to_thread_items_for_projection(message: &Message) -> Vec<Value> {
    match message {
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            let mut items = Vec::new();
            if content.is_some() || reasoning_content.is_some() || tool_calls.is_empty() {
                items.push(message_to_thread_json(message));
            }
            items.extend(tool_calls.iter().map(tool_call_to_thread_item));
            items
        }
        Message::Tool {
            tool_call_id,
            content,
            terminal,
            ..
        } => vec![tool_result_to_thread_item(
            tool_call_id,
            content,
            terminal.as_ref(),
        )],
        _ => vec![message_to_thread_json(message)],
    }
}

fn legacy_stored_message_to_thread_items_for_projection(message: &StoredMessage) -> Vec<Value> {
    match message {
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            let mut items = Vec::new();
            if content.is_some() || reasoning_content.is_some() || tool_calls.is_empty() {
                items.push(stored_message_to_thread_json(message));
            }
            items.extend(tool_calls.iter().map(tool_call_to_thread_item));
            items
        }
        StoredMessage::Tool {
            tool_call_id,
            content,
            terminal,
            ..
        } => vec![tool_result_to_thread_item(
            tool_call_id,
            content,
            terminal.terminal_ref(),
        )],
        _ => vec![stored_message_to_thread_json(message)],
    }
}

fn merge_projected_items(turn_items: &mut Vec<Value>, items: Vec<Value>) {
    for item in items {
        if item["type"] == "tool_result"
            && let Some(tool_call_id) = item["toolCallId"].as_str()
            && let Some(existing) = turn_items
                .iter_mut()
                .rev()
                .find(|candidate| projected_tool_item_matches_result(candidate, tool_call_id))
        {
            complete_tool_item(existing, &item);
            continue;
        }
        turn_items.push(item);
    }
}

fn projected_tool_item_matches_result(candidate: &Value, tool_call_id: &str) -> bool {
    candidate["id"].as_str() == Some(tool_call_id)
        || (candidate["type"] == "fileChange"
            && candidate["id"].as_str() == Some(&format!("{tool_call_id}:file-change")))
}

fn tool_call_to_thread_item(tool_call: &RawToolCall) -> Value {
    if let Some((server, tool)) = mcp_tool_parts(&tool_call.function_name) {
        mcp_tool_started_item(
            tool_call.id.clone(),
            server,
            tool,
            parse_json_or_null(&tool_call.arguments),
        )
    } else {
        let arguments = parse_json_or_null(&tool_call.arguments);
        if let Some(item) =
            persisted_file_change_started_item(&tool_call.id, &tool_call.function_name, &arguments)
        {
            return item;
        }
        if matches!(tool_call.function_name.as_str(), "bash" | "exec_command") {
            return command_execution_thread_item(tool_call);
        }
        dynamic_tool_started_item(
            tool_call.id.clone(),
            tool_call.function_name.clone(),
            arguments,
        )
    }
}

fn command_execution_thread_item(tool_call: &RawToolCall) -> Value {
    persisted_command_execution_started_item(
        tool_call.id.clone(),
        tool_call.function_name.clone(),
        command_from_tool_arguments(&tool_call.arguments),
    )
}

fn tool_result_to_thread_item(
    tool_call_id: &str,
    content: &str,
    terminal: Option<&ToolTerminal>,
) -> Value {
    let mut item = json!({
        "type": "tool_result",
        "toolCallId": tool_call_id,
        "content": content,
    });
    if let Some(terminal) = terminal {
        item["status"] = Value::from(terminal.status.as_str());
        if let Some(error) = &terminal.error {
            item["error"] = Value::from(error.clone());
        }
        if let Some(exit_code) = terminal.exit_code {
            item["exitCode"] = Value::from(exit_code);
        }
        if terminal.truncated {
            item["truncated"] = Value::from(true);
        }
        if let Value::Object(metadata) = ProjectedToolTerminalMetadata::from(terminal).into_value()
        {
            item.as_object_mut()
                .expect("tool result projection is an object")
                .extend(metadata);
        }
    }
    item
}

fn complete_tool_item(item: &mut Value, result: &Value) {
    complete_projected_tool_item(item, result);
}

fn command_from_tool_arguments(raw: &str) -> Value {
    let arguments = parse_json_or_null(raw);
    arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))
        .and_then(Value::as_str)
        .map(|command| Value::from(command.to_string()))
        .unwrap_or(Value::Null)
}

fn items_for_view(items_view: TurnItemsView, items: Vec<Value>) -> Vec<Value> {
    match items_view {
        TurnItemsView::NotLoaded => Vec::new(),
        TurnItemsView::Summary | TurnItemsView::Full => items,
    }
}

fn turn_id_for_index(index: usize) -> String {
    format!("turn-{}", index + 1)
}

fn item_id_for_index(index: usize) -> String {
    format!("item-{}", index + 1)
}

impl From<SessionSummary> for StoredThreadSummary {
    fn from(summary: SessionSummary) -> Self {
        Self {
            thread_id: summary.session_id,
            title: summary.title,
            cwd: summary.cwd,
            provider: summary.provider,
            model: summary.model,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            archived: summary.archived,
            parent_id: summary.parent_id,
            forked: summary.forked,
            approval_mode: summary.approval_mode,
            active_permission_profile: summary.active_permission_profile,
            permission_rule_count: summary.permission_rule_count,
            runtime_workspace_roots: summary.runtime_workspace_roots,
            additional_working_directories: summary.additional_working_directories,
            network_domain_permissions: summary.network_domain_permissions,
            health: summary.health,
            health_issue: summary.health_issue,
            source_fingerprint: summary.source_fingerprint,
            storage_identity: summary.storage_identity,
        }
    }
}
