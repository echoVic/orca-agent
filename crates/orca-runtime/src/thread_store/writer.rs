#[cfg(test)]
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use orca_core::conversation::{Message, SummaryState};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventPublicationStore, EventType};
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_core::thread_identity::{ConversationItemId, TurnId};
use orca_core::thread_item_projection::CompletedModelResponse;
use orca_core::tool_types::ToolResult;
use orca_platform::fs::{
    AtomicWritePolicy, ExclusiveFileLock, atomic_write_with, open_nofollow_nonblocking,
};

use crate::history::{self, CompactionRecord, ContextSummaryRecord};

#[cfg(not(test))]
use super::session_index;
use super::types::{
    ManualCompactionDurableSnapshot, ManualCompactionSnapshotRecord, SessionMeta, SessionRecord,
    SessionTranscript, StoredConversationRecord, StoredMessage,
};

#[cfg(test)]
static MANUAL_COMPACTION_SNAPSHOT_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static MANUAL_COMPACTION_SNAPSHOT_POST_WRITE_FAILURES: OnceLock<Mutex<HashSet<PathBuf>>> =
    OnceLock::new();

pub(crate) fn read_manual_compaction_snapshot(
    path: &Path,
    operation_id: &crate::runtime_surface::SurfaceOperationId,
) -> io::Result<Option<ManualCompactionDurableSnapshot>> {
    let record = read_records(path)?.into_iter().rev().find_map(|record| {
        let SessionRecord::ManualCompactionSnapshot(record) = record else {
            return None;
        };
        (record.operation_id == *operation_id).then_some(record)
    });
    Ok(record.map(|record| ManualCompactionDurableSnapshot {
        operation_id: record.operation_id,
        strategy: record.strategy,
        before_messages: record.before_messages,
        conversation: orca_core::conversation::Conversation {
            messages: record.messages.into_iter().map(Message::from).collect(),
            internal_context: Default::default(),
            rolling_summary: record.rolling_summary,
            summary: record.summary_state,
        },
    }))
}

pub(crate) fn read_prompt_queue_snapshot(
    path: &Path,
) -> io::Result<crate::prompt_queue::PromptQueueSnapshot> {
    Ok(read_records(path)?
        .into_iter()
        .rev()
        .find_map(|record| match record {
            SessionRecord::PromptQueueSnapshot(snapshot) => Some(snapshot),
            _ => None,
        })
        .unwrap_or_default())
}

pub(crate) fn write_prompt_queue_snapshot(
    path: &Path,
    snapshot: &crate::prompt_queue::PromptQueueSnapshot,
) -> io::Result<()> {
    write_durable_record(path, &SessionRecord::PromptQueueSnapshot(snapshot.clone()))
}

pub(crate) fn write_record(path: &Path, record: &SessionRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let _lock = acquire_file_lock(path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    write_record_line(&mut file, record)?;
    file.flush()?;
    #[cfg(not(test))]
    {
        match record {
            SessionRecord::Meta(meta) => {
                let _ = session_index::upsert_meta(path, meta, false);
            }
            SessionRecord::Completed { .. } => {
                let _ = session_index::touch_path_force(path);
            }
            _ => {
                let _ = session_index::touch_path(path);
            }
        }
    }
    Ok(())
}

pub(crate) fn write_durable_record(path: &Path, record: &SessionRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = acquire_file_lock(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    repair_incomplete_final_record(&mut file)?;
    write_record_line(&mut file, record)?;
    file.flush()?;
    file.sync_data()?;
    #[cfg(not(test))]
    let _ = session_index::touch_path(path);
    Ok(())
}

fn repair_incomplete_final_record(file: &mut File) -> io::Result<()> {
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        file.seek(SeekFrom::End(0))?;
        return Ok(());
    }

    file.seek(SeekFrom::End(-1))?;
    let mut final_byte = [0; 1];
    file.read_exact(&mut final_byte)?;
    if final_byte[0] == b'\n' {
        file.seek(SeekFrom::End(0))?;
        return Ok(());
    }

    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file);
    let mut line = Vec::new();
    let mut line_start = 0_u64;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line_start = line_start
                .checked_add(read as u64)
                .ok_or_else(|| io::Error::other("JSONL offset overflow"))?;
            continue;
        }
        break;
    }
    drop(reader);

    match serde_json::from_slice::<SessionRecord>(&line) {
        Ok(_) => {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        }
        Err(error) if error.classify() == serde_json::error::Category::Eof => {
            file.set_len(line_start)?;
            file.seek(SeekFrom::End(0))?;
        }
        Err(error) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid complete final JSONL record: {error}"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn write_record_line(mut writer: impl Write, record: &SessionRecord) -> io::Result<()> {
    let redacted = redact_session_record(record);
    let mut line = serde_json::to_string(&redacted).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes())
}

pub(crate) fn read_records(path: &Path) -> io::Result<Vec<SessionRecord>> {
    let lines = read_history_lines(path)?;
    let mut records = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionRecord>(line) {
            Ok(SessionRecord::Message { id, turn_id, .. }) if id.is_some() != turn_id.is_some() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid conversation identity at {} line {}: id and turn_id must be present together",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Ok(SessionRecord::SemanticEvent { event }) => {
                conversation_record_from_semantic_event(&event).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid semantic event record at {} line {}: {error}",
                            path.display(),
                            i + 1
                        ),
                    )
                })?;
                records.push(SessionRecord::SemanticEvent { event });
            }
            Ok(record) => records.push(record),
            Err(error) if line_has_record_type(line, "event.semantic") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid semantic event record at {} line {}: {error}",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Err(error) if line_has_invalid_tool_terminal(line) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid session record at {} line {}: {error}",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Err(error) if line_has_conversation_identity(line) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid conversation identity at {} line {}: {error}",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Err(_) if i == lines.len() - 1 => break,
            Err(_) => continue,
        }
    }
    Ok(records)
}

pub(crate) fn conversation_record_from_semantic_event(
    event: &EventEnvelope,
) -> io::Result<Option<StoredConversationRecord>> {
    if event.event_type != EventType::ModelResponseCompleted {
        return Ok(None);
    }
    let response = serde_json::from_value::<CompletedModelResponse>(event.payload.clone())
        .map_err(io::Error::other)?;
    Ok(Some(StoredConversationRecord::completed_model_response(
        &response,
    )))
}

fn line_has_conversation_identity(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
        value["type"] == "conversation.message"
            && (!value["id"].is_null() || !value["turn_id"].is_null())
    })
}

fn line_has_record_type(line: &str, record_type: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| value["type"] == record_type)
}

fn line_has_invalid_tool_terminal(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if value["type"].as_str() != Some("conversation.message")
        || value["message"]["role"].as_str() != Some("tool")
    {
        return false;
    }
    super::types::validate_stored_tool_terminal_fields(&value["message"])
        .is_some_and(|result| result.is_err())
}

pub(crate) fn rewrite_records_unlocked(path: &Path, records: &[SessionRecord]) -> io::Result<()> {
    atomic_write_with(path, AtomicWritePolicy::NoFollow, |temporary| {
        write_records_to(temporary, path, records)
    })
    .map_err(io::Error::other)?;
    #[cfg(not(test))]
    let _ = session_index::touch_path(path);
    Ok(())
}

fn write_records_to(
    file: &mut File,
    target_path: &Path,
    records: &[SessionRecord],
) -> io::Result<()> {
    if target_path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        let mut encoder = zstd::stream::write::Encoder::new(file, 3)?;
        for record in records {
            write_record_line(&mut encoder, record)?;
        }
        encoder.finish()?;
    } else {
        let mut writer = io::BufWriter::new(file);
        for record in records {
            write_record_line(&mut writer, record)?;
        }
        writer.flush()?;
    }
    Ok(())
}

pub(crate) fn read_history_lines(path: &Path) -> io::Result<Vec<String>> {
    open_history_reader(path)?.lines().collect()
}

pub(crate) fn open_regular_history_file(path: &Path) -> io::Result<File> {
    let file = open_nofollow_nonblocking(path).map_err(io::Error::other)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session history is not a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

pub(crate) fn open_history_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = open_regular_history_file(path)?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        let decoder = zstd::stream::read::Decoder::new(file)?;
        return Ok(Box::new(BufReader::new(decoder)));
    }
    Ok(Box::new(BufReader::new(file)))
}

pub(crate) fn read_session_meta(path: &Path) -> io::Result<SessionMeta> {
    let reader = open_history_reader(path)?;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(SessionRecord::Meta(meta)) = serde_json::from_str::<SessionRecord>(&line) {
            return Ok(meta);
        }
        break;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("missing session metadata in {}", path.display()),
    ))
}

pub(crate) fn read_transcript(path: &Path) -> io::Result<SessionTranscript> {
    transcript_from_records(path, read_records(path)?)
}

/// Read a transcript but restore only the message log up to the persisted
/// conversation item id `boundary_message_id` (inclusive). Records after the
/// boundary — including uncommitted tool calls — are not replayed.
pub(crate) fn read_transcript_until(
    path: &Path,
    boundary_message_id: &str,
) -> io::Result<SessionTranscript> {
    let records = read_records(path)?;
    transcript_from_records(
        path,
        truncate_records_at_boundary(records, boundary_message_id)?,
    )
}

/// Keep records through the last `conversation.message` whose item id matches
/// `boundary_message_id`; drop everything after it.
pub(crate) fn truncate_records_at_boundary(
    records: Vec<SessionRecord>,
    boundary_message_id: &str,
) -> io::Result<Vec<SessionRecord>> {
    let mut boundary_index = None;
    for (index, record) in records.iter().enumerate() {
        if let SessionRecord::Message { id: Some(id), .. } = record
            && id.as_str() == boundary_message_id
        {
            boundary_index = Some(index);
        }
    }
    let Some(boundary_index) = boundary_index else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved message matches '{boundary_message_id}'"),
        ));
    };
    Ok(records.into_iter().take(boundary_index + 1).collect())
}

/// Apply a message boundary to an already-loaded transcript by re-reading the
/// records from its durable path, so preloaded transcripts honor the boundary
/// exactly like freshly loaded ones.
pub(crate) fn truncate_transcript_at_boundary(
    transcript: &SessionTranscript,
    boundary_message_id: &str,
) -> io::Result<SessionTranscript> {
    let records = read_records(&transcript.path)?;
    transcript_from_records(
        &transcript.path,
        truncate_records_at_boundary(records, boundary_message_id)?,
    )
}

pub(crate) fn transcript_from_records(
    path: &Path,
    records: Vec<SessionRecord>,
) -> io::Result<SessionTranscript> {
    let mut meta = None;
    let mut messages = Vec::new();
    let mut compactions = Vec::new();
    let mut summaries = Vec::new();
    let mut usage_baseline = UsageTotals::default();
    let mut foreground_usage = None;
    let mut background_usage = UsageTotals::default();
    let mut has_usage_baseline = false;
    let mut has_background_usage = false;
    let mut last_plan: Option<(Option<String>, Vec<PlanItem>)> = None;
    let mut completion_status = None;
    let mut completion_error = None;
    let mut next_event_seq = 0;
    let mut semantic_events = Vec::new();

    for record in records {
        match record {
            SessionRecord::Meta(m) => meta = Some(m),
            SessionRecord::Message { message, .. } => {
                messages.push(message.into());
            }
            SessionRecord::Completed { status, error, .. } => {
                completion_status = Some(status);
                completion_error = error;
            }
            SessionRecord::BackgroundTaskProviderResponse {
                usage: Some(record),
                ..
            } => {
                background_usage.input_tokens = background_usage
                    .input_tokens
                    .saturating_add(record.input_tokens);
                background_usage.output_tokens = background_usage
                    .output_tokens
                    .saturating_add(record.output_tokens);
                background_usage.cache_tokens = background_usage
                    .cache_tokens
                    .saturating_add(record.cache_tokens);
                background_usage.estimated_cost_usd += record.estimated_cost_usd;
                has_background_usage = true;
            }
            SessionRecord::BackgroundTaskProviderResponse { usage: None, .. } => {}
            SessionRecord::ContextCollapsed(record) => compactions.push(record),
            SessionRecord::ContextSummary(record) => summaries.push(record),
            SessionRecord::ManualCompactionSnapshot(record) => {
                let after_messages = record.messages.len();
                messages = record.messages.into_iter().map(Message::from).collect();
                compactions.clear();
                summaries.clear();
                if record.rolling_summary.is_some() || !record.summary_state.is_empty() {
                    summaries.push(ContextSummaryRecord {
                        summarized_at: record.compacted_at,
                        before_messages: record.before_messages,
                        after_messages,
                        summary: record.rolling_summary.unwrap_or_default(),
                        summary_state: Some(record.summary_state),
                    });
                }
            }
            SessionRecord::Usage(record) => foreground_usage = Some(record),
            SessionRecord::UsageBaseline(record) => {
                usage_baseline = record;
                foreground_usage = None;
                background_usage = UsageTotals::default();
                has_usage_baseline = true;
                has_background_usage = false;
            }
            SessionRecord::EventSequenceReserved { next_seq } => {
                next_event_seq = next_event_seq.max(next_seq);
            }
            SessionRecord::SemanticEvent { event } => {
                if let Some(record) = conversation_record_from_semantic_event(&event)? {
                    messages.push(record.message.into());
                }
                semantic_events.push(event);
            }
            SessionRecord::SurfaceCommitPrepared { .. }
            | SessionRecord::SurfaceCommitCommitted { .. }
            | SessionRecord::SurfaceOwnerEpoch { .. }
            | SessionRecord::SurfaceFinalizeIntent { .. }
            | SessionRecord::SurfaceSettlement { .. }
            | SessionRecord::SurfaceShutdownBarrier { .. }
            | SessionRecord::PromptQueueSnapshot(_) => {}
            SessionRecord::PlanState { explanation, plan } => {
                let all_done = !plan.is_empty()
                    && plan.iter().all(|item| item.status == PlanStatus::Completed);
                if plan.is_empty() || all_done {
                    last_plan = None;
                } else {
                    last_plan = Some((explanation, plan));
                }
            }
            SessionRecord::Checkpoint(_) | SessionRecord::PromptCacheCheckpoint(_) => {}
        }
    }

    let meta = meta.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("missing session metadata in {}", path.display()),
        )
    })?;
    let has_foreground_usage = foreground_usage.is_some();
    let mut aggregate_usage = usage_baseline;
    add_usage_totals(&mut aggregate_usage, foreground_usage.unwrap_or_default());
    add_usage_totals(&mut aggregate_usage, background_usage);
    let usage = (has_usage_baseline || has_foreground_usage || has_background_usage)
        .then_some(aggregate_usage);

    Ok(SessionTranscript {
        meta,
        messages,
        compactions,
        summaries,
        usage,
        plan: last_plan,
        completion_status,
        completion_error,
        next_event_seq,
        semantic_events,
        path: path.to_path_buf(),
    })
}

pub(crate) fn read_latest_context_tokens(path: &Path) -> io::Result<Option<u64>> {
    let mut previous_input_tokens = 0;
    let mut latest_context_tokens = None;
    for record in read_records(path)? {
        match record {
            SessionRecord::Usage(usage) => {
                latest_context_tokens = Some(
                    usage
                        .input_tokens
                        .checked_sub(previous_input_tokens)
                        .unwrap_or(usage.input_tokens),
                );
                previous_input_tokens = usage.input_tokens;
            }
            SessionRecord::UsageBaseline(_) => previous_input_tokens = 0,
            _ => {}
        }
    }
    Ok(latest_context_tokens)
}

fn redact_session_record(record: &SessionRecord) -> SessionRecord {
    let mut redacted = record.clone();
    match &mut redacted {
        SessionRecord::Meta(meta) => {
            redact_string_in_place(&mut meta.cwd);
            redact_string_in_place(&mut meta.provider);
            if let Some(model) = &mut meta.model {
                redact_string_in_place(model);
            }
            redact_string_in_place(&mut meta.title);
            if let Some(parent_id) = &mut meta.parent_id {
                redact_string_in_place(parent_id);
            }
        }
        SessionRecord::Message { message, .. } => redact_stored_message(message),
        SessionRecord::Completed { status, error, .. } => {
            redact_string_in_place(status);
            if let Some(error) = error {
                redact_string_in_place(error);
            }
        }
        SessionRecord::BackgroundTaskProviderResponse {
            task_id,
            status,
            error,
            ..
        } => {
            redact_string_in_place(task_id);
            redact_string_in_place(status);
            if let Some(error) = error {
                redact_string_in_place(error);
            }
        }
        SessionRecord::ContextCollapsed(_) => {}
        SessionRecord::ContextSummary(record) => {
            redact_string_in_place(&mut record.summary);
            if let Some(summary_state) = &mut record.summary_state {
                if let Some(baseline) = &mut summary_state.baseline {
                    redact_string_in_place(baseline);
                }
                for delta in &mut summary_state.deltas {
                    redact_string_in_place(delta);
                }
            }
        }
        SessionRecord::ManualCompactionSnapshot(record) => {
            for message in &mut record.messages {
                redact_stored_message(message);
            }
            if let Some(summary) = &mut record.rolling_summary {
                redact_string_in_place(summary);
            }
            if let Some(baseline) = &mut record.summary_state.baseline {
                redact_string_in_place(baseline);
            }
            for delta in &mut record.summary_state.deltas {
                redact_string_in_place(delta);
            }
        }
        SessionRecord::Usage(_)
        | SessionRecord::UsageBaseline(_)
        | SessionRecord::EventSequenceReserved { .. } => {}
        SessionRecord::SemanticEvent { event } => redact_json_value(&mut event.payload),
        SessionRecord::SurfaceCommitPrepared { .. }
        | SessionRecord::SurfaceCommitCommitted { .. }
        | SessionRecord::SurfaceOwnerEpoch { .. }
        | SessionRecord::SurfaceSettlement { .. } => {}
        SessionRecord::SurfaceFinalizeIntent { .. }
        | SessionRecord::SurfaceShutdownBarrier { .. } => {}
        SessionRecord::PromptQueueSnapshot(_) => {}
        SessionRecord::PlanState { explanation, plan } => {
            if let Some(explanation) = explanation {
                redact_string_in_place(explanation);
            }
            for item in plan {
                redact_string_in_place(&mut item.step);
            }
        }
        SessionRecord::Checkpoint(record) => {
            redact_string_in_place(&mut record.session_id);
            redact_string_in_place(&mut record.status);
            if let Some(reason) = &mut record.reason {
                redact_string_in_place(reason);
            }
            if let Some(task_plan) = &mut record.task_plan {
                redact_string_in_place(task_plan);
            }
        }
        SessionRecord::PromptCacheCheckpoint(_) => {}
    }
    redacted
}

fn add_usage_totals(totals: &mut UsageTotals, usage: UsageTotals) {
    totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
    totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
    totals.cache_tokens = totals.cache_tokens.saturating_add(usage.cache_tokens);
    totals.estimated_cost_usd += usage.estimated_cost_usd;
}

fn redact_stored_message(message: &mut StoredMessage) {
    match message {
        StoredMessage::System { content, .. } | StoredMessage::User { content, .. } => {
            redact_string_in_place(content)
        }
        StoredMessage::Tool {
            content, terminal, ..
        } => {
            redact_string_in_place(content);
            if let Some(error) = terminal.error_mut() {
                redact_string_in_place(error);
            }
        }
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            if let Some(content) = content {
                redact_string_in_place(content);
            }
            if let Some(reasoning_content) = reasoning_content {
                redact_string_in_place(reasoning_content);
            }
            for tool_call in tool_calls {
                redact_string_in_place(&mut tool_call.arguments);
            }
        }
    }
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => redact_string_in_place(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_json_value(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_string_in_place(value: &mut String) {
    *value = redact_sensitive_text(value);
}

pub(crate) fn redact_sensitive_text(value: &str) -> String {
    redact_standalone_secret_tokens(&redact_keyed_secret_values(value))
}

fn redact_keyed_secret_values(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'=' && bytes[index] != b':' {
            index += 1;
            continue;
        }

        let key_start = key_start_before_delimiter(bytes, index);
        let key = trim_key_candidate(&value[key_start..index]);
        if !is_sensitive_key(key) {
            index += 1;
            continue;
        }

        let mut value_start = index + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let quote = (value_start < bytes.len()
            && (bytes[value_start] == b'"' || bytes[value_start] == b'\''))
            .then(|| bytes[value_start]);
        let content_start = if quote.is_some() {
            value_start + 1
        } else {
            value_start
        };
        let content_end = if let Some(quote) = quote {
            quoted_value_end(bytes, content_start, quote)
        } else {
            unquoted_value_end(bytes, content_start)
        };
        if content_start == content_end {
            index += 1;
            continue;
        }

        output.push_str(&value[cursor..content_start]);
        output.push_str("<redacted>");
        if quote.is_some() && content_end < bytes.len() {
            output.push(bytes[content_end] as char);
            cursor = content_end + 1;
            index = cursor;
        } else {
            cursor = content_end;
            index = cursor;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn key_start_before_delimiter(bytes: &[u8], delimiter_index: usize) -> usize {
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

fn trim_key_candidate(key: &str) -> &str {
    key.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '[' | ','))
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("authorization")
}

fn quoted_value_end(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return index;
        }
        index += 1;
    }
    index
}

fn unquoted_value_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b',' | b'}' | b']' | b';')
    {
        index += 1;
    }
    index
}

fn redact_standalone_secret_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut token = String::new();
    for ch in value.chars() {
        if is_secret_token_boundary(ch) {
            push_redacted_token(&mut output, &mut token);
            output.push(ch);
        } else {
            token.push(ch);
        }
    }
    push_redacted_token(&mut output, &mut token);
    output
}

fn is_secret_token_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '}' | '[' | ']' | ',' | ':')
}

fn push_redacted_token(output: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if looks_like_standalone_secret(token) {
        output.push_str("<redacted>");
    } else {
        output.push_str(token);
    }
    token.clear();
}

fn looks_like_standalone_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '.' | ';' | ')' | '(' | '<' | '>' | '`' | '"' | '\'' | '='
        )
    });
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("sk-")
        || (trimmed.len() >= 16
            && lower.contains("secret")
            && (lower.contains("token")
                || lower.contains("password")
                || lower.contains("key")
                || lower.contains("test")))
}

pub(crate) fn acquire_file_lock(path: &Path) -> io::Result<ExclusiveFileLock> {
    let logical_path = if path.extension().and_then(|extension| extension.to_str()) == Some("zst") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    let mut lock_path = logical_path.as_os_str().to_os_string();
    lock_path.push(".lock");
    ExclusiveFileLock::acquire(Path::new(&lock_path)).map_err(io::Error::other)
}

#[derive(Clone, Debug)]
pub struct SessionWriter {
    path: PathBuf,
    conversation_records: Arc<Mutex<Vec<StoredConversationRecord>>>,
    event_sequence_cursor: Arc<Mutex<u64>>,
    turn_id: Option<TurnId>,
    session_id: Option<String>,
}

fn restore_plaintext_transcript(path: PathBuf) -> io::Result<PathBuf> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
        return Ok(path);
    }
    let plain_path = path.with_extension("");
    let _lock = acquire_file_lock(&path)?;
    let result = (|| {
        let input = open_regular_history_file(&path)?;
        let output = File::create(&plain_path)?;
        if let Err(error) = zstd::stream::copy_decode(input, output) {
            let _ = fs::remove_file(&plain_path);
            return Err(io::Error::other(error));
        }
        fs::remove_file(&path)?;
        #[cfg(not(test))]
        {
            let archived = plain_path.starts_with(super::local::archive_dir());
            let _ = session_index::move_path(&path, &plain_path, archived);
        }
        Ok(plain_path)
    })();
    result
}

impl SessionWriter {
    #[cfg(test)]
    pub(crate) fn inject_manual_compaction_snapshot_failure_once(path: impl Into<PathBuf>) {
        MANUAL_COMPACTION_SNAPSHOT_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }

    #[cfg(test)]
    pub(crate) fn inject_manual_compaction_snapshot_post_write_failure_once(
        path: impl Into<PathBuf>,
    ) {
        MANUAL_COMPACTION_SNAPSHOT_POST_WRITE_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.into());
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The session id of this transcript, when known (from the session meta
    /// record). `None` for transcripts without one.
    pub(crate) fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    /// The last committed conversation item id (the durable resume boundary)
    /// written to this transcript, when one exists. Message records are
    /// appended with their item id before any checkpoint references them.
    pub(crate) fn last_committed_message_id(&self) -> Option<String> {
        self.conversation_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .find_map(|record| record.item_id.as_ref().map(|id| id.as_str().to_string()))
    }

    pub fn start(
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::start_from_meta(history::create_meta(cwd, provider, model, prompt))
    }

    pub fn start_from_meta(meta: SessionMeta) -> io::Result<Self> {
        let session_id = meta.session_id.clone();
        let path = history::session_path(&meta.session_id, meta.created_at)?;
        write_record(&path, &SessionRecord::Meta(meta))?;
        Ok(Self {
            path,
            conversation_records: Arc::new(Mutex::new(Vec::new())),
            event_sequence_cursor: Arc::new(Mutex::new(0)),
            turn_id: None,
            session_id: Some(session_id),
        })
    }

    pub fn append_to_existing(path: PathBuf) -> io::Result<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("history file not found: {}", path.display()),
            ));
        }
        // Appends write plaintext JSONL; raw bytes after a zstd frame would
        // make the whole transcript undecodable, so a compressed session must
        // be restored to plaintext before it can be continued.
        let path = restore_plaintext_transcript(path)?;
        #[cfg(not(test))]
        if let Ok(meta) = read_session_meta(&path) {
            let archived = path.starts_with(super::local::archive_dir());
            let _ = session_index::upsert_meta(&path, &meta, archived);
        }
        append_usage_baseline(&path)?;
        let event_sequence_cursor = read_transcript(&path)?.next_event_seq;
        let mut conversation_records = Vec::new();
        let mut session_id = None;
        for record in read_records(&path)? {
            match record {
                SessionRecord::Meta(meta) => session_id = Some(meta.session_id),
                SessionRecord::Message {
                    id,
                    turn_id,
                    message,
                } => conversation_records.push(StoredConversationRecord {
                    item_id: id,
                    turn_id,
                    message,
                    completed_model_items: None,
                }),
                SessionRecord::SemanticEvent { event } => {
                    if let Some(record) = conversation_record_from_semantic_event(&event)? {
                        conversation_records.push(record);
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            path,
            conversation_records: Arc::new(Mutex::new(conversation_records)),
            event_sequence_cursor: Arc::new(Mutex::new(event_sequence_cursor)),
            turn_id: None,
            session_id,
        })
    }

    pub fn enter_turn(&mut self, turn_id: TurnId) {
        self.turn_id = Some(turn_id);
    }

    pub(crate) fn conversation_records(&self) -> Vec<StoredConversationRecord> {
        self.conversation_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn append_message(&mut self, message: &Message) -> io::Result<()> {
        let message = StoredMessage::from(message);
        let record = match &self.turn_id {
            Some(turn_id) => StoredConversationRecord::identified(
                ConversationItemId::new(),
                turn_id.clone(),
                message,
            ),
            None if matches!(message, StoredMessage::System { .. }) => {
                StoredConversationRecord::legacy(message)
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "conversation message append requires an active turn identity",
                ));
            }
        };
        self.append_conversation_record(record)
    }

    pub(crate) fn append_legacy_message(&mut self, message: &Message) -> io::Result<()> {
        self.append_conversation_record(StoredConversationRecord::legacy(StoredMessage::from(
            message,
        )))
    }

    pub(crate) fn append_detached_message(&mut self, message: &Message) -> io::Result<()> {
        self.append_conversation_record(StoredConversationRecord::identified(
            ConversationItemId::new(),
            TurnId::new(),
            StoredMessage::from(message),
        ))
    }

    pub fn append_tool_result_message(
        &mut self,
        result: &ToolResult,
        content: String,
        pinned: bool,
    ) -> io::Result<()> {
        let turn_id = self.turn_id.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tool result append requires an active turn identity",
            )
        })?;
        self.append_conversation_record(StoredConversationRecord::identified(
            ConversationItemId::new(),
            turn_id,
            StoredMessage::Tool {
                tool_call_id: result.id.clone(),
                content,
                terminal: super::types::StoredToolTerminal::from_terminal(Some(result.terminal())),
                pinned,
            },
        ))
    }

    fn append_conversation_record(&mut self, record: StoredConversationRecord) -> io::Result<()> {
        let mut records = self
            .conversation_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_record(&self.path, &record.as_session_record())?;
        records.push(record);
        Ok(())
    }

    pub fn complete(&mut self, status: &str) -> io::Result<()> {
        self.complete_with_error(status, None)
    }

    pub fn complete_with_error(&mut self, status: &str, error: Option<&str>) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::Completed {
                status: status.to_string(),
                completed_at: Utc::now(),
                error: error.map(str::to_string),
            },
        )
    }

    pub(crate) fn append_checkpoint(
        &mut self,
        checkpoint: super::types::SessionCheckpointRecord,
    ) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::Checkpoint(checkpoint))
    }

    pub fn append_prompt_cache_checkpoint(
        &mut self,
        turn_id: orca_core::thread_identity::TurnId,
        checkpoint: orca_provider::prompt_cache::PromptCacheCheckpoint,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::PromptCacheCheckpoint(
                super::types::SessionPromptCacheCheckpointRecord {
                    turn_id,
                    checkpoint,
                    recorded_at: Utc::now(),
                },
            ),
        )
    }

    pub fn append_background_task_provider_response(
        &mut self,
        task_id: &str,
        status: &str,
        error: Option<&str>,
        usage: Option<UsageTotals>,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::BackgroundTaskProviderResponse {
                task_id: task_id.to_string(),
                status: status.to_string(),
                completed_at: Utc::now(),
                error: error.map(str::to_string),
                usage,
            },
        )
    }

    pub fn append_compaction(
        &mut self,
        before_messages: usize,
        after_messages: usize,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextCollapsed(CompactionRecord {
                collapsed_at: Utc::now(),
                before_messages,
                after_messages,
            }),
        )
    }

    pub fn append_summary(
        &mut self,
        before_messages: usize,
        after_messages: usize,
        summary: impl Into<String>,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextSummary(ContextSummaryRecord {
                summarized_at: Utc::now(),
                before_messages,
                after_messages,
                summary: summary.into(),
                summary_state: None,
            }),
        )
    }

    pub fn append_summary_state(
        &mut self,
        before_messages: usize,
        after_messages: usize,
        summary: impl Into<String>,
        summary_state: &SummaryState,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextSummary(ContextSummaryRecord {
                summarized_at: Utc::now(),
                before_messages,
                after_messages,
                summary: summary.into(),
                summary_state: Some(summary_state.clone()),
            }),
        )
    }

    pub(crate) fn append_manual_compaction_snapshot(
        &mut self,
        identity: &crate::session::ManualCompactionPersistenceIdentity,
        before_messages: usize,
        strategy: &str,
        conversation: &orca_core::conversation::Conversation,
    ) -> io::Result<()> {
        #[cfg(test)]
        if MANUAL_COMPACTION_SNAPSHOT_FAILURES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.path)
        {
            return Err(io::Error::other(
                "injected manual compaction snapshot failure",
            ));
        }
        let record = SessionRecord::ManualCompactionSnapshot(ManualCompactionSnapshotRecord {
            snapshot_id: identity.snapshot_id.clone(),
            operation_id: identity.operation_id.clone(),
            strategy: strategy.to_string(),
            compacted_at: Utc::now(),
            before_messages,
            messages: conversation
                .messages
                .iter()
                .map(StoredMessage::from)
                .collect(),
            rolling_summary: conversation.rolling_summary.clone(),
            summary_state: conversation.summary.clone(),
        });
        let result = write_durable_record(&self.path, &record);
        #[cfg(test)]
        let result = if result.is_ok()
            && MANUAL_COMPACTION_SNAPSHOT_POST_WRITE_FAILURES
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.path)
        {
            Err(io::Error::other(
                "injected manual compaction post-write failure",
            ))
        } else {
            result
        };
        if result.is_ok() {
            return Ok(());
        }
        let exact_record_present = read_records(&self.path).is_ok_and(|records| {
            records.into_iter().any(|candidate| {
                matches!(
                    candidate,
                    SessionRecord::ManualCompactionSnapshot(snapshot)
                        if snapshot.snapshot_id == identity.snapshot_id
                            && snapshot.operation_id == identity.operation_id
                )
            })
        });
        if !exact_record_present {
            return result;
        }
        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        file.sync_data()
    }

    pub fn append_usage(&mut self, usage: UsageTotals) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::Usage(usage))
    }

    fn reserve_event_sequence(&self, next_seq: u64) -> io::Result<()> {
        let mut expected = self
            .event_sequence_cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _lock = acquire_file_lock(&self.path)?;
        let current = read_transcript(&self.path)?.next_event_seq;
        if current != *expected {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "stale event sequence reservation: expected {}, current reservation is {current}; another process may be writing this session",
                    *expected
                ),
            ));
        }
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        write_record_line(
            &mut file,
            &SessionRecord::EventSequenceReserved { next_seq },
        )?;
        file.flush()?;
        #[cfg(not(test))]
        let _ = session_index::touch_path(&self.path);
        *expected = (*expected).max(next_seq);
        Ok(())
    }

    fn append_semantic_event_record(&self, event: &EventEnvelope) -> io::Result<()> {
        let record = conversation_record_from_semantic_event(event)?;
        if let Some(record) = record {
            let mut records = self
                .conversation_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            write_record(
                &self.path,
                &SessionRecord::SemanticEvent {
                    event: event.clone(),
                },
            )?;
            records.push(record);
            return Ok(());
        }
        write_record(
            &self.path,
            &SessionRecord::SemanticEvent {
                event: event.clone(),
            },
        )
    }

    pub fn append_plan_state(
        &mut self,
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    ) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::PlanState { explanation, plan })
    }
}

impl EventPublicationStore for SessionWriter {
    fn reserve_through(&self, next_seq_exclusive: u64) -> io::Result<()> {
        self.reserve_event_sequence(next_seq_exclusive)
    }

    fn append_semantic_event(&self, event: &EventEnvelope) -> io::Result<()> {
        self.append_semantic_event_record(event)
    }
}

fn append_usage_baseline(path: &Path) -> io::Result<()> {
    let _lock = acquire_file_lock(path)?;
    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    let result = (|| {
        let Some(usage) = read_transcript(path)?.usage else {
            return Ok(());
        };
        write_record_line(&mut file, &SessionRecord::UsageBaseline(usage))?;
        file.flush()?;
        #[cfg(not(test))]
        let _ = session_index::touch_path(path);
        Ok(())
    })();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::event_schema::{EVENT_SCHEMA_VERSION, EventType};
    use orca_core::thread_identity::TurnId;
    use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
    use orca_provider::prompt_cache::PromptCacheCheckpoint;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const APPEND_PROBE_PATH: &str = "ORCA_THREAD_STORE_APPEND_PROBE_PATH";
    const APPEND_PROBE_WORKER: &str = "ORCA_THREAD_STORE_APPEND_PROBE_WORKER";
    const APPEND_PROBE_COUNT: &str = "ORCA_THREAD_STORE_APPEND_PROBE_COUNT";

    fn usage(input_tokens: u64, output_tokens: u64, cache_tokens: u64, cost: f64) -> UsageTotals {
        UsageTotals {
            input_tokens,
            output_tokens,
            cache_tokens,
            estimated_cost_usd: cost,
        }
    }

    fn assert_usage(actual: UsageTotals, expected: UsageTotals) {
        assert_eq!(actual.input_tokens, expected.input_tokens);
        assert_eq!(actual.output_tokens, expected.output_tokens);
        assert_eq!(actual.cache_tokens, expected.cache_tokens);
        assert!(
            (actual.estimated_cost_usd - expected.estimated_cost_usd).abs() < 1e-12,
            "expected cost {}, got {}",
            expected.estimated_cost_usd,
            actual.estimated_cost_usd
        );
    }

    #[test]
    fn prompt_queue_snapshot_preserves_input_required_for_recovery_bits_spec_ut() {
        let path = tempfile::NamedTempFile::new().expect("temporary prompt queue store");
        let recovery_input = "recover token=test-queue-recovery-value";
        let snapshot = crate::prompt_queue::PromptQueueSnapshot {
            revision: crate::prompt_queue::QueueRevision::from_u64(3),
            paused: true,
            items: vec![crate::prompt_queue::QueuedSubmission {
                id: crate::prompt_queue::QueuedSubmissionId::new(),
                client_user_message_id: crate::prompt_queue::ClientUserMessageId::new(),
                input: crate::prompt_queue::PromptQueueInput::text(recovery_input),
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            }],
            dispatch: None,
        };

        write_prompt_queue_snapshot(path.path(), &snapshot).expect("persist prompt queue snapshot");
        let recovered =
            read_prompt_queue_snapshot(path.path()).expect("recover prompt queue snapshot");

        assert_eq!(recovered, snapshot);
        assert_eq!(recovered.items[0].input.text, recovery_input);
    }

    #[test]
    fn prompt_cache_checkpoint_record_round_trips_without_changing_transcript() {
        let (_directory, path, mut writer) = new_transcript();
        let turn_id = TurnId::new();
        writer.enter_turn(turn_id.clone());
        writer
            .append_message(&Message::user("visible message".to_string()))
            .expect("write conversation message");
        writer
            .append_usage(usage(11, 3, 7, 0.01))
            .expect("write usage");
        writer.complete("success").expect("write completion");
        let before = read_transcript(&path).expect("read baseline transcript");

        writer
            .append_prompt_cache_checkpoint(
                turn_id,
                PromptCacheCheckpoint {
                    version: 1,
                    scope_sha256: "a".repeat(64),
                    message_prefix_sha256: "b".repeat(64),
                    message_count: 1,
                    tool_schema_sha256: "c".repeat(64),
                    tool_count: 0,
                },
            )
            .expect("append cache checkpoint");

        let raw = fs::read_to_string(&path).expect("read JSONL");
        assert!(raw.contains("\"type\":\"provider.prompt_cache_checkpoint\""));
        let records = read_records(&path).expect("read checkpoint record");
        assert!(records.iter().any(|record| {
            matches!(record, SessionRecord::PromptCacheCheckpoint(record)
                if record.checkpoint.message_count == 1 && record.checkpoint.tool_count == 0)
        }));

        let after = read_transcript(&path).expect("read transcript with checkpoint");
        assert_eq!(after.messages.len(), before.messages.len());
        assert_eq!(after.usage, before.usage);
        assert_eq!(after.completion_status, before.completion_status);
        assert_eq!(after.completion_error, before.completion_error);
    }

    #[cfg(unix)]
    #[test]
    fn history_reader_rejects_fifo_without_waiting_for_a_writer() {
        use std::ffi::CString;
        use std::sync::mpsc;

        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let fifo = directory.path().join("blocked.jsonl");
        let fifo_path = CString::new(fifo.as_os_str().as_encoded_bytes()).expect("FIFO path");
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(open_history_reader(&fifo).is_err());
        });

        assert!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("history reader waited for FIFO writer"),
            "history reader accepted a FIFO"
        );
    }

    fn new_transcript() -> (tempfile::TempDir, PathBuf, SessionWriter) {
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let path = directory.path().join("resume-usage.jsonl");
        let meta = SessionMeta {
            schema_version: 1,
            session_id: "resume-usage".to_string(),
            cwd: directory.path().display().to_string(),
            provider: "mock".to_string(),
            model: None,
            title: "resume usage".to_string(),
            created_at: Utc::now(),
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            metadata_writable_directories: Vec::new(),
            network_domain_permissions: Default::default(),
            unsandboxed_shell: false,
        };
        write_record(&path, &SessionRecord::Meta(meta)).expect("write metadata");
        let writer = SessionWriter {
            path: path.clone(),
            conversation_records: Arc::new(Mutex::new(Vec::new())),
            event_sequence_cursor: Arc::new(Mutex::new(0)),
            turn_id: None,
            session_id: None,
        };
        (directory, path, writer)
    }

    #[test]
    fn concurrent_process_appends_keep_every_jsonl_record_intact() {
        let (_directory, path, _writer) = new_transcript();
        let workers = 4_u64;
        let records_per_worker = 40_u64;
        let mut children = (0..workers)
            .map(|worker| spawn_append_probe(&path, worker, records_per_worker))
            .collect::<Vec<_>>();

        for child in &mut children {
            wait_for_append_probe(child, Duration::from_secs(10));
        }

        let values = read_records(&path)
            .expect("read concurrent transcript")
            .into_iter()
            .filter_map(|record| match record {
                SessionRecord::Usage(usage) => Some(usage.input_tokens),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(values.len() as u64, workers * records_per_worker);
        for worker in 0..workers {
            for record in 0..records_per_worker {
                assert!(values.contains(&(worker * 1_000 + record)));
            }
        }
        assert_eq!(
            fs::read(&path).expect("read transcript bytes").last(),
            Some(&b'\n')
        );
    }

    #[test]
    fn blocked_append_reopens_the_rewritten_transcript_after_lock_release() {
        let (_directory, path, _writer) = new_transcript();
        let lock = acquire_file_lock(&path).expect("hold transcript lock");
        let mut child = spawn_append_probe(&path, 9, 1);

        thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().expect("inspect blocked writer").is_none(),
            "append probe bypassed the transcript sidecar lock"
        );

        rewrite_records_unlocked(&path, &[SessionRecord::Usage(usage(7, 0, 0, 0.0))])
            .expect("rewrite transcript while owning lock");
        drop(lock);
        wait_for_append_probe(&mut child, Duration::from_secs(10));

        let values = read_records(&path)
            .expect("read rewritten transcript")
            .into_iter()
            .filter_map(|record| match record {
                SessionRecord::Usage(usage) => Some(usage.input_tokens),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![7, 9_000]);
    }

    #[test]
    fn compressed_and_plaintext_transcript_paths_share_one_process_lock() {
        let (_directory, path, _writer) = new_transcript();
        let compressed_path = path.with_extension("jsonl.zst");
        let lock = acquire_file_lock(&compressed_path).expect("hold compressed transcript lock");
        let mut child = spawn_append_probe(&path, 8, 1);

        thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().expect("inspect blocked writer").is_none(),
            "plaintext append bypassed the compressed transcript lock"
        );

        drop(lock);
        wait_for_append_probe(&mut child, Duration::from_secs(10));

        let values = read_records(&path)
            .expect("read transcript after compressed lock release")
            .into_iter()
            .filter_map(|record| match record {
                SessionRecord::Usage(usage) => Some(usage.input_tokens),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![8_000]);
    }

    #[test]
    fn latest_context_tokens_are_the_delta_between_usage_snapshots() {
        let (_directory, path, mut writer) = new_transcript();
        writer
            .append_usage(usage(929_128, 10_260, 893_696, 0.06))
            .expect("write penultimate usage snapshot");
        writer
            .append_usage(usage(970_611, 10_627, 935_040, 0.06))
            .expect("write latest usage snapshot");

        assert_eq!(read_latest_context_tokens(&path).unwrap(), Some(41_483));
    }

    #[test]
    fn thread_store_append_probe_child() {
        let Some(path) = std::env::var_os(APPEND_PROBE_PATH).map(PathBuf::from) else {
            return;
        };
        let worker = std::env::var(APPEND_PROBE_WORKER)
            .expect("append probe worker")
            .parse::<u64>()
            .expect("numeric append probe worker");
        let count = std::env::var(APPEND_PROBE_COUNT)
            .expect("append probe count")
            .parse::<u64>()
            .expect("numeric append probe count");
        for record in 0..count {
            write_record(
                &path,
                &SessionRecord::Usage(usage(worker * 1_000 + record, 0, 0, 0.0)),
            )
            .expect("append probe record");
        }
    }

    fn spawn_append_probe(path: &Path, worker: u64, count: u64) -> Child {
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "thread_store::writer::tests::thread_store_append_probe_child",
                "--nocapture",
            ])
            .env(APPEND_PROBE_PATH, path)
            .env(APPEND_PROBE_WORKER, worker.to_string())
            .env(APPEND_PROBE_COUNT, count.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn append probe")
    }

    fn wait_for_append_probe(child: &mut Child, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("poll append probe") {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut stream) = child.stdout.take() {
                    stream.read_to_end(&mut stdout).expect("read probe stdout");
                }
                if let Some(mut stream) = child.stderr.take() {
                    stream.read_to_end(&mut stderr).expect("read probe stderr");
                }
                assert!(
                    status.success(),
                    "append probe failed: status={status}, stdout={}, stderr={}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("append probe timed out");
    }

    #[test]
    fn manual_compaction_snapshot_is_one_atomic_resume_boundary() {
        let (_directory, path, mut writer) = new_transcript();
        writer.enter_turn(TurnId::new());
        writer
            .append_message(&Message::user("discarded prefix".to_string()))
            .expect("append prefix");
        writer
            .append_message(&Message::Assistant {
                content: Some("discarded answer".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })
            .expect("append answer");

        let mut compacted = orca_core::conversation::Conversation::new();
        compacted.add_system(
            "[Earlier conversation history was truncated to fit context window]".to_string(),
        );
        compacted.add_user("retained tail".to_string());
        compacted.rolling_summary = Some("rolling summary".to_string());
        compacted.summary = orca_core::conversation::SummaryState {
            baseline: Some("baseline".to_string()),
            deltas: vec!["delta".to_string()],
        };
        let identity = crate::session::ManualCompactionPersistenceIdentity {
            operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7"),
            snapshot_id: uuid::Uuid::now_v7().to_string(),
        };
        SessionWriter::inject_manual_compaction_snapshot_post_write_failure_once(path.clone());
        writer
            .append_manual_compaction_snapshot(&identity, 4, "local_truncation", &compacted)
            .expect("post-write ambiguity is reprobed and durably acknowledged");
        writer
            .append_message(&Message::Assistant {
                content: Some("post-compaction answer".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })
            .expect("append post-compaction message");

        let records = read_records(&path).expect("read manual compaction records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, SessionRecord::ManualCompactionSnapshot(_)))
                .count(),
            1
        );
        let durable = read_manual_compaction_snapshot(&path, &identity.operation_id)
            .expect("read manual compaction receipt")
            .expect("manual compaction receipt exists");
        assert_eq!(durable.operation_id, identity.operation_id);
        assert_eq!(durable.strategy, "local_truncation");
        assert_eq!(durable.before_messages, 4);
        let transcript = read_transcript(&path).expect("resume manual compaction snapshot");
        assert!(matches!(
            transcript.messages.as_slice(),
            [
                Message::System { content: marker, .. },
                Message::User { content: tail, .. },
                Message::Assistant {
                    content: Some(answer),
                    ..
                }
            ] if marker == "[Earlier conversation history was truncated to fit context window]"
                && tail == "retained tail"
                && answer == "post-compaction answer"
        ));
        let summary = transcript.summaries.as_slice();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].before_messages, 4);
        assert_eq!(summary[0].after_messages, 2);
        assert_eq!(summary[0].summary, "rolling summary");
        let state = summary[0]
            .summary_state
            .as_ref()
            .expect("summary state survives atomic snapshot");
        assert_eq!(state.baseline.as_deref(), Some("baseline"));
        assert_eq!(state.deltas, ["delta"]);
    }

    #[test]
    fn writer_clones_share_record_order_but_keep_independent_turn_scopes() {
        let (_directory, path, mut foreground) = new_transcript();
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        foreground.enter_turn(first_turn.clone());
        let mut background = foreground.clone();
        foreground.enter_turn(second_turn.clone());

        background
            .append_message(&Message::user("background first".to_string()))
            .expect("append background record");
        foreground
            .append_message(&Message::user("foreground second".to_string()))
            .expect("append foreground record");

        let ledger = foreground.conversation_records();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].turn_id.as_ref(), Some(&first_turn));
        assert_eq!(ledger[1].turn_id.as_ref(), Some(&second_turn));
        assert_ne!(ledger[0].item_id, ledger[1].item_id);
        assert_eq!(background.conversation_records().len(), 2);

        let persisted = read_records(&path)
            .expect("read persisted records")
            .into_iter()
            .filter_map(|record| match record {
                SessionRecord::Message { id, turn_id, .. } => Some((id, turn_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            persisted,
            ledger
                .iter()
                .map(|record| (record.item_id.clone(), record.turn_id.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn completed_model_response_is_one_semantic_record_and_one_derived_ledger_entry() {
        let (_directory, path, writer) = new_transcript();
        let identity = ModelResponseIdentity::new(TurnId::new());
        let response = CompletedModelResponse::new(
            identity.clone(),
            Some("completed answer".to_string()),
            Some("completed reasoning".to_string()),
            Vec::new(),
        );
        let event = EventEnvelope {
            version: EVENT_SCHEMA_VERSION.to_string(),
            run_id: "resume-usage".to_string(),
            seq: 0,
            timestamp_ms: 1,
            event_type: EventType::ModelResponseCompleted,
            payload: serde_json::to_value(&response).expect("serialize completed response"),
        };

        writer
            .append_semantic_event(&event)
            .expect("append completed response");

        let records = read_records(&path).expect("read completed response records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, SessionRecord::SemanticEvent { .. }))
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, SessionRecord::Message { .. }))
                .count(),
            0
        );
        let ledger = writer.conversation_records();
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger[0].item_id.as_ref(),
            Some(&identity.item_ids.conversation_item_id)
        );
        assert_eq!(ledger[0].turn_id.as_ref(), Some(&identity.turn_id));
        assert_eq!(
            ledger[0]
                .completed_model_items
                .as_ref()
                .expect("canonical model items"),
            &response.completed_items()
        );
        assert!(matches!(
            read_transcript(&path)
                .expect("read completed response transcript")
                .messages
                .as_slice(),
            [Message::Assistant { content: Some(content), reasoning_content: Some(reasoning), .. }]
                if content == "completed answer" && reasoning == "completed reasoning"
        ));
    }

    #[test]
    fn failed_message_append_never_enters_the_shared_ledger() {
        let (directory, _path, mut writer) = new_transcript();
        writer.enter_turn(TurnId::new());
        writer.path = directory.path().to_path_buf();

        writer
            .append_message(&Message::user("must fail".to_string()))
            .expect_err("directory path cannot accept a transcript append");

        assert!(writer.conversation_records().is_empty());
    }

    #[test]
    fn detached_message_identity_does_not_replace_the_foreground_turn_scope() {
        let (_directory, _path, mut writer) = new_transcript();
        let foreground_turn = TurnId::new();
        writer.enter_turn(foreground_turn.clone());

        writer
            .append_detached_message(&Message::pinned_user("context".to_string()))
            .expect("append detached context");
        writer
            .append_message(&Message::user("foreground".to_string()))
            .expect("append foreground message");

        let records = writer.conversation_records();
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].turn_id.as_ref(), Some(&foreground_turn));
        assert_eq!(records[1].turn_id.as_ref(), Some(&foreground_turn));
    }

    #[test]
    fn redaction_and_compaction_preserve_conversation_identity() {
        let (_directory, path, mut writer) = new_transcript();
        let turn_id = TurnId::new();
        writer.enter_turn(turn_id.clone());
        writer
            .append_message(&Message::user(
                "use token=sk-test-identity-redaction-1234567890".to_string(),
            ))
            .expect("append redacted conversation record");
        let before_compaction = writer.conversation_records();
        let item_id = before_compaction[0]
            .item_id
            .clone()
            .expect("conversation item id");

        writer
            .append_compaction(1, 1)
            .expect("append compaction record");

        let after_compaction = writer.conversation_records();
        assert_eq!(after_compaction[0].item_id.as_ref(), Some(&item_id));
        assert_eq!(after_compaction[0].turn_id.as_ref(), Some(&turn_id));
        let persisted = read_records(&path)
            .expect("read redacted compacted transcript")
            .into_iter()
            .find_map(|record| match record {
                SessionRecord::Message {
                    id,
                    turn_id,
                    message,
                } => Some((id, turn_id, message)),
                _ => None,
            })
            .expect("persisted conversation record");
        assert_eq!(persisted.0.as_ref(), Some(&item_id));
        assert_eq!(persisted.1.as_ref(), Some(&turn_id));
        assert!(
            matches!(persisted.2, StoredMessage::User { content, .. } if content.contains("<redacted>"))
        );
    }

    #[test]
    fn read_records_rejects_partial_or_malformed_conversation_identity() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        let item_id = ConversationItemId::new();
        fs::write(
            path.path(),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "conversation.message",
                    "id": item_id,
                    "message": { "role": "user", "content": "partial" }
                })
            ),
        )
        .expect("write partial identity");
        assert!(
            read_records(path.path())
                .expect_err("partial identity must fail closed")
                .to_string()
                .contains("id and turn_id must be present together")
        );

        fs::write(
            path.path(),
            concat!(
                "{\"type\":\"conversation.message\",",
                "\"id\":\"item_not-a-uuid\",",
                "\"turn_id\":\"turn_not-a-uuid\",",
                "\"message\":{\"role\":\"user\",\"content\":\"malformed\"}}\n"
            ),
        )
        .expect("write malformed identity");
        assert!(
            read_records(path.path())
                .expect_err("malformed identity must fail closed")
                .to_string()
                .contains("invalid conversation identity")
        );
    }

    fn aggregate_usage(path: &Path) -> UsageTotals {
        read_transcript(path)
            .expect("read transcript")
            .usage
            .expect("aggregate usage")
    }

    #[test]
    fn transcript_reduces_event_sequence_reservations_to_exclusive_maximum() {
        let (_directory, path, writer) = new_transcript();

        writer.reserve_through(256).expect("reserve first block");
        writer.reserve_through(512).expect("reserve second block");
        writer
            .reserve_through(256)
            .expect("stale reservation remains readable");

        let transcript = read_transcript(&path).expect("read sequence reservation");
        assert_eq!(transcript.next_event_seq, 512);
        let records = read_records(&path).expect("read typed records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, SessionRecord::EventSequenceReserved { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn semantic_event_round_trips_as_the_original_typed_envelope() {
        let (_directory, path, writer) = new_transcript();
        let event = EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "semantic-round-trip".to_string(),
            seq: 37,
            timestamp_ms: 1_234_567,
            event_type: orca_core::event_schema::EventType::ToolCallCompleted,
            payload: serde_json::json!({
                "id": "tool-1",
                "name": "shell",
                "status": "completed",
                "nested": { "preserved": true }
            }),
        };

        writer
            .append_semantic_event(&event)
            .expect("append semantic event");

        let raw = fs::read_to_string(&path).expect("read semantic JSONL");
        let semantic_line = raw
            .lines()
            .find(|line| line.contains("\"type\":\"event.semantic\""))
            .expect("semantic JSONL line");
        serde_json::from_str::<SessionRecord>(semantic_line).expect("parse typed semantic record");
        let transcript = read_transcript(&path).expect("read semantic transcript");
        assert_eq!(
            transcript.semantic_events.as_slice(),
            std::slice::from_ref(&event)
        );
        let records = read_records(&path).expect("read typed semantic record");
        assert!(records.iter().any(|record| {
            matches!(record, SessionRecord::SemanticEvent { event: stored } if stored == &event)
        }));
        let record = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record["type"] == "event.semantic")
            .expect("semantic JSONL record");
        assert_eq!(record["event"], serde_json::to_value(event).unwrap());
    }

    #[test]
    fn semantic_event_payload_uses_recursive_history_redaction() {
        let (_directory, path, writer) = new_transcript();
        let event = EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "semantic-redaction".to_string(),
            seq: 0,
            timestamp_ms: 42,
            event_type: orca_core::event_schema::EventType::ToolCallRequested,
            payload: serde_json::json!({
                "raw_arguments": {
                    "authorization": "token=secret-test-value",
                    "nested": ["api_key=secret-test-key"]
                }
            }),
        };

        writer
            .append_semantic_event(&event)
            .expect("append redacted semantic event");

        let transcript = read_transcript(&path).expect("read redacted semantic transcript");
        let payload = &transcript.semantic_events[0].payload;
        assert_eq!(
            payload["raw_arguments"]["authorization"],
            "token=<redacted>"
        );
        assert_eq!(payload["raw_arguments"]["nested"][0], "api_key=<redacted>");
    }

    #[test]
    fn sensitive_key_without_a_value_is_safe_during_streaming_redaction() {
        assert_eq!(redact_sensitive_text("pending token:"), "pending token:");
        assert_eq!(
            redact_sensitive_text("pending api_key=   "),
            "pending api_key=   "
        );
    }

    #[test]
    fn legacy_transcript_without_event_sequence_reservation_starts_at_zero() {
        let (_directory, path, _writer) = new_transcript();

        let transcript = read_transcript(&path).expect("read legacy transcript");

        assert_eq!(transcript.next_event_seq, 0);
        assert!(transcript.semantic_events.is_empty());
    }

    #[test]
    fn stale_event_sequence_reservation_is_rejected() {
        let (_directory, path, writer) = new_transcript();
        let stale_writer =
            SessionWriter::append_to_existing(path.clone()).expect("open concurrent writer");
        writer
            .reserve_event_sequence(orca_core::event_schema::EVENT_SEQUENCE_RESERVATION_SIZE)
            .expect("first sequence reservation");

        let error = stale_writer
            .reserve_event_sequence(orca_core::event_schema::EVENT_SEQUENCE_RESERVATION_SIZE)
            .expect_err("a second writer with the same cursor must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            error
                .to_string()
                .contains("stale event sequence reservation")
        );
        assert_eq!(
            read_transcript(&path)
                .expect("read transcript")
                .next_event_seq,
            orca_core::event_schema::EVENT_SEQUENCE_RESERVATION_SIZE
        );
    }

    fn seed_foreground_and_background(writer: &mut SessionWriter) {
        writer
            .append_usage(usage(100, 20, 40, 0.10))
            .expect("write initial foreground usage");
        writer
            .append_background_task_provider_response(
                "background-1",
                "success",
                None,
                Some(usage(30, 10, 15, 0.05)),
            )
            .expect("write background usage");
    }

    #[test]
    fn legacy_transcript_without_baseline_keeps_background_usage() {
        let (_directory, path, mut writer) = new_transcript();
        seed_foreground_and_background(&mut writer);
        writer
            .append_usage(usage(140, 25, 50, 0.13))
            .expect("update foreground snapshot");

        assert_usage(aggregate_usage(&path), usage(170, 35, 65, 0.18));
    }

    #[test]
    fn usage_baseline_accumulates_later_background_delta() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut resumed = SessionWriter::append_to_existing(path.clone()).expect("resume writer");
        resumed
            .append_background_task_provider_response(
                "background-2",
                "success",
                None,
                Some(usage(20, 5, 8, 0.03)),
            )
            .expect("write resumed background usage");

        assert_usage(aggregate_usage(&path), usage(150, 35, 63, 0.18));
    }

    #[test]
    fn usage_baseline_uses_latest_resumed_foreground_snapshot() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut resumed = SessionWriter::append_to_existing(path.clone()).expect("resume writer");
        resumed
            .append_usage(usage(50, 8, 20, 0.04))
            .expect("write resumed foreground usage");
        resumed
            .append_usage(usage(80, 12, 30, 0.07))
            .expect("update resumed foreground snapshot");

        assert_usage(aggregate_usage(&path), usage(210, 42, 85, 0.22));
    }

    #[test]
    fn multiple_resumes_roll_forward_each_aggregate_baseline_once() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut first_resume =
            SessionWriter::append_to_existing(path.clone()).expect("first resume writer");
        first_resume
            .append_usage(usage(80, 12, 30, 0.07))
            .expect("write first resumed foreground usage");

        let mut second_resume =
            SessionWriter::append_to_existing(path.clone()).expect("second resume writer");
        second_resume
            .append_usage(usage(20, 4, 7, 0.02))
            .expect("write second resumed foreground usage");

        assert_usage(aggregate_usage(&path), usage(230, 46, 92, 0.24));
        assert_eq!(
            read_records(&path)
                .expect("read records")
                .iter()
                .filter(|record| matches!(record, SessionRecord::UsageBaseline(_)))
                .count(),
            2
        );
    }

    #[test]
    fn read_records_rejects_conflicting_tool_terminal_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        let valid = serde_json::to_string(&SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::User {
                content: "valid".to_string(),
                pinned: false,
            },
        })
        .expect("serialize valid record");
        fs::write(
            path.path(),
            format!(
                "{valid}\n{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\",\"tool_call_id\":\"call-1\",\"content\":\"cancelled\",\"status\":\"cancelled\",\"kind\":\"runtime_error\"}}}}\n{valid}\n"
            ),
        )
        .expect("write transcript");

        let error = read_records(path.path()).expect_err("terminal conflict must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn read_records_rejects_complete_invalid_semantic_event_record() {
        let (_directory, path, _writer) = new_transcript();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open semantic transcript");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "event.semantic",
                "event": {
                    "version": "1",
                    "run_id": "invalid-semantic",
                    "seq": 0,
                    "timestamp_ms": "not-a-number",
                    "type": "error",
                    "payload": { "message": "invalid" }
                }
            })
        )
        .expect("write invalid semantic event");

        let error =
            read_records(&path).expect_err("known invalid semantic record must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("invalid semantic event record"));
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn read_records_ignores_only_truncated_final_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        let valid = serde_json::to_string(&SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::User {
                content: "valid".to_string(),
                pinned: false,
            },
        })
        .expect("serialize valid record");
        fs::write(
            path.path(),
            format!("{valid}\n{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\""),
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("truncated final record is recoverable");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn read_records_preserves_legacy_skip_for_unrelated_invalid_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        fs::write(
            path.path(),
            "{\"type\":\"conversation.message\",\"message\":{\"role\":\"tool\"\n",
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("legacy malformed tail is skipped");
        assert!(records.is_empty());
    }

    #[test]
    fn read_records_skips_terminal_shaped_record_with_unrelated_error() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        fs::write(
            path.path(),
            concat!(
                "{\"type\":\"conversation.message\",\"message\":{\"role\":\"tool\",\"content\":\"missing id\",\"status\":\"cancelled\",\"kind\":\"cancelled\"}}\n",
                "{\"type\":\"conversation.message\",\"message\":{\"role\":\"user\",\"content\":\"valid\"}}\n",
            ),
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("unrelated old bad record is skipped");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn read_records_rejects_invalid_terminal_metadata_beyond_status_kind_conflicts() {
        for invalid_fields in [
            "\"kind\":\"cancelled\"",
            "\"status\":\"cancelled\",\"kind\":\"cancelled\",\"terminal_source\":\"future\"",
            "\"status\":\"cancelled\",\"kind\":\"cancelled\",\"invocation_started\":42",
        ] {
            let path = tempfile::NamedTempFile::new().expect("temp transcript");
            fs::write(
                path.path(),
                format!(
                    "{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\",\"tool_call_id\":\"call-1\",\"content\":\"cancelled\",{invalid_fields}}}}}\n"
                ),
            )
            .expect("write transcript");

            let error = read_records(path.path()).expect_err("terminal metadata must fail closed");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }
}
