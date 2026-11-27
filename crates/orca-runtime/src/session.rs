use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;
use std::io;
use std::path::Path;

use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::{
    Conversation, Message, REPEAT_TOOL_REMINDER_PREFIX, assistant_message_has_payload,
    is_internal_image_analysis,
};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::TaskStatus;
use orca_core::thread_item_projection::CompletedModelResponse;
use orca_core::tool_types::{
    ToolInvocationStarted, ToolName, ToolRequest, ToolResult, ToolStatus, ToolTerminal,
    ToolTerminalSource,
};
use orca_mcp::McpRegistry;

use crate::agent_common;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::instructions::{self, ProjectInstructions};
use crate::memory::{self, MemoryBlock};
use crate::tasks::TaskRegistry;
use crate::thread_store::{
    SessionMeta, SessionStore, SessionTranscript, SessionWriter, StoredConversationRecord,
    ThreadStore,
};

const INTERRUPTED_RESUME_HINT: &str = "Previous turn was interrupted; existing workspace edits remain. Inspect git diff before continuing.";

pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("run-{nanos}-{}", uuid::Uuid::new_v4())
}

/// The logical turn boundary survives provider suspension and generation changes.
#[derive(Clone)]
pub(crate) struct TurnStart {
    pub(crate) message_index: usize,
    pub(crate) file_write_baseline: HashMap<String, u64>,
    pub(crate) workspace_baseline: crate::workspace_observation::Snapshot,
}

pub struct InteractiveSession {
    store: SessionStore,
    conversation: Conversation,
    writer: Option<SessionWriter>,
    session_id: Option<String>,
    completion_error: Option<String>,
    instructions: ProjectInstructions,
    cost_tracker: CostTracker,
    usage_baseline: UsageTotals,
    mcp_registry: McpRegistry,
    hooks: HookRunner,
    memory: MemoryBlock,
    auto_memory_worker: Option<memory::AutomaticMemoryWorker>,
    turn_starts: HashMap<String, TurnStart>,
    completion_context: Option<serde_json::Value>,
    history_requested: bool,
    task_registry: TaskRegistry,
    last_manual_compaction: Option<ManualCompactionOutcome>,
    runtime_extensions: Option<std::sync::Arc<crate::extension::ExtensionData>>,
}

pub(crate) struct InteractiveSessionRuntimeParts<'a> {
    pub conversation: &'a mut Conversation,
    pub writer: Option<&'a mut SessionWriter>,
    pub recorded: bool,
    pub instructions: &'a ProjectInstructions,
    pub cost_tracker: &'a mut CostTracker,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub memory: &'a MemoryBlock,
    pub task_registry: &'a TaskRegistry,
}

#[derive(Clone, Debug)]
pub(crate) struct ManualCompactionOutcome {
    pub before: Conversation,
    pub after: Conversation,
    pub strategy: &'static str,
    pub observation: orca_provider::context::CompactionObservation,
    pub usage: Option<UsageTotals>,
}

#[derive(Clone, Debug)]
pub(crate) struct ManualCompactionPersistenceIdentity {
    pub operation_id: crate::runtime_surface::SurfaceOperationId,
    pub snapshot_id: String,
}

pub(crate) fn record_tool_result_for_agent(
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    result: &ToolResult,
    emit_deltas: bool,
) -> io::Result<bool> {
    let result_content = agent_common::format_tool_result_for_model(result);
    let message = Message::Tool {
        tool_call_id: result.id.clone(),
        content: result_content.clone(),
        images: result.images.clone(),
        terminal: Some(result.terminal().clone()),
        pinned: false,
    };
    conversation.add_tool_result_with_terminal(result, result_content);
    crate::instructions::queue_tool_touches(conversation, result);
    let reminders = settled_repeat_tool_reminders(conversation);
    let reminder_messages = reminders
        .iter()
        .cloned()
        .map(Message::system)
        .collect::<Vec<_>>();
    conversation
        .messages
        .extend(reminder_messages.iter().cloned());
    // Persistence is a durability concern, not an execution concern: a broken
    // transcript must not stop the tool turn. The item is already in memory, so
    // record the failure, mark the writer degraded, and let the turn continue.
    if emit_deltas
        && let Some(writer) = history_writer
        && let Err(error) = writer.append_message(&message).and_then(|()| {
            for reminder in &reminder_messages {
                writer.append_message(reminder)?;
            }
            Ok(())
        })
    {
        tracing::warn!(
            path = %writer.path().display(),
            %error,
            "session history write failed; continuing with an in-memory tool result"
        );
        eprintln!(
            "orca: warning: session history write failed ({}); continuing in degraded mode: {error}",
            writer.path().display()
        );
        writer.mark_degraded();
        return Ok(false);
    }
    Ok(true)
}

fn settled_repeat_tool_reminders(conversation: &mut Conversation) -> Vec<String> {
    fn is_poll(name: &str) -> bool {
        matches!(name, "task_wait" | "task_read_output" | "task_list")
    }

    fn finished(terminal: &ToolTerminal) -> bool {
        terminal.source == ToolTerminalSource::Observed
            && terminal.started != ToolInvocationStarted::No
            && matches!(terminal.status, ToolStatus::Completed | ToolStatus::Failed)
    }

    fn canonical_arguments(arguments: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(arguments) {
            Ok(mut value) => {
                value.sort_all_objects();
                value.to_string()
            }
            Err(_) => arguments.to_owned(),
        }
    }

    // Reconstruct only the streak that can still produce a reminder. The
    // transcript already gives each thread its own call order and user boundary.
    let Some((calls, results)) = settled_tool_group(&conversation.messages) else {
        return Vec::new();
    };
    let mut reminders = Vec::new();
    for call in calls {
        if is_poll(&call.function_name) {
            continue;
        }
        let Some(result) = results
            .iter()
            .find(|(id, _)| id == &call.id)
            .map(|(_, result)| result)
        else {
            continue;
        };
        if !finished(result) {
            continue;
        }
        let arguments = canonical_arguments(&call.arguments);
        let streak = match conversation.repeat_tool_streak.as_mut() {
            Some(streak)
                if streak.tool_name == call.function_name && streak.arguments == arguments =>
            {
                streak.count = streak.count.saturating_add(1);
                streak
            }
            _ => {
                conversation.repeat_tool_streak = Some(orca_core::conversation::RepeatToolStreak {
                    tool_name: call.function_name.clone(),
                    arguments: arguments.clone(),
                    count: 1,
                });
                conversation
                    .repeat_tool_streak
                    .as_mut()
                    .expect("streak set")
            }
        };
        if matches!(streak.count, 3 | 5 | 8) {
            let mut reminder = format!(
                "{REPEAT_TOOL_REMINDER_PREFIX}{} with the same arguments {} times in a row. The result will not change; use what you already have, change the arguments, or try a different approach.",
                streak.tool_name, streak.count
            );
            if streak.count > 3 {
                reminder.push_str(" Arguments: ");
                reminder.extend(streak.arguments.chars().take(300));
            }
            reminders.push(reminder);
        }
    }
    reminders
}

fn settled_tool_group(
    messages: &[Message],
) -> Option<(
    Vec<orca_core::conversation::RawToolCall>,
    Vec<(String, orca_core::tool_types::ToolTerminal)>,
)> {
    let assistant_index = messages.iter().rposition(|message| {
        matches!(message, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty())
    })?;
    let Message::Assistant { tool_calls, .. } = &messages[assistant_index] else {
        unreachable!();
    };
    let mut results = Vec::new();
    for message in &messages[assistant_index + 1..] {
        match message {
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } => results.push((tool_call_id.clone(), terminal.clone())),
            _ => break,
        }
    }
    if tool_calls
        .iter()
        .any(|call| !results.iter().any(|(id, _)| id == &call.id))
    {
        return None;
    }
    Some((tool_calls.clone(), results))
}

pub(crate) fn repeat_tool_streak_from_history(
    messages: &[Message],
) -> Option<orca_core::conversation::RepeatToolStreak> {
    fn canonical_arguments(arguments: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(arguments) {
            Ok(mut value) => {
                value.sort_all_objects();
                value.to_string()
            }
            Err(_) => arguments.to_owned(),
        }
    }

    let mut streak: Option<orca_core::conversation::RepeatToolStreak> = None;
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User {
                content, images, ..
            } if !images.is_empty() || !is_internal_image_analysis(content) => {
                streak = None;
                index += 1;
            }
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let Some((calls, results)) = settled_tool_group(&messages[index..]) else {
                    break;
                };
                for call in calls {
                    if matches!(
                        call.function_name.as_str(),
                        "task_wait" | "task_read_output" | "task_list"
                    ) {
                        continue;
                    }
                    let Some((_, terminal)) = results.iter().find(|(id, _)| id == &call.id) else {
                        continue;
                    };
                    if terminal.source != ToolTerminalSource::Observed
                        || terminal.started == ToolInvocationStarted::No
                        || !matches!(terminal.status, ToolStatus::Completed | ToolStatus::Failed)
                    {
                        continue;
                    }
                    let arguments = canonical_arguments(&call.arguments);
                    match streak.as_mut() {
                        Some(streak)
                            if streak.tool_name == call.function_name
                                && streak.arguments == arguments =>
                        {
                            streak.count = streak.count.saturating_add(1);
                        }
                        _ => {
                            streak = Some(orca_core::conversation::RepeatToolStreak {
                                tool_name: call.function_name,
                                arguments,
                                count: 1,
                            });
                        }
                    }
                }
                index += 1 + results.len();
            }
            _ => index += 1,
        }
    }
    streak
}

pub(crate) fn record_assistant_response_for_agent<W: io::Write>(
    conversation: &mut Conversation,
    response: &CompletedModelResponse,
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<()> {
    if !assistant_message_has_payload(response.assistant_content.as_deref(), &response.tool_calls) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "assistant response did not contain content or tool calls",
        ));
    }
    if emit_deltas && let Err(error) = sink.emit(events.model_response_completed(response)) {
        // As with tool results, the assistant turn is live state first. A
        // failed event/session append must not prevent it from entering the
        // conversation; the caller's semantic persistence boundary still
        // reports its own failures separately.
        tracing::warn!(%error, "assistant response event write failed; keeping the in-memory response");
        eprintln!(
            "orca: warning: assistant response event write failed; keeping the in-memory response: {error}"
        );
    }
    conversation.internal_context.note_assistant_turn();
    conversation.messages.push(response.assistant_message());
    Ok(())
}

pub(crate) fn bootstrap_agent_conversation(
    resumed: Option<&SessionTranscript>,
    system_prompt: Message,
    cwd: &Path,
    prompt: &str,
) -> Conversation {
    let mut conversation = if let Some(resumed) = resumed {
        let mut conv = crate::history::resume_conversation_in_cwd(
            resumed,
            system_prompt.content_str().unwrap_or_default().to_owned(),
            cwd,
        );
        conv.strip_legacy_pinned_volatile();
        conv.strip_legacy_summary_messages();
        conv.unpin_delivered_notices();
        crate::instructions::refresh_nested_instructions(&mut conv, cwd);
        conv
    } else {
        let mut conversation = Conversation::new();
        conversation.messages.push(system_prompt);
        conversation
    };
    if resumed.and_then(|transcript| transcript.completion_status.as_deref()) == Some("interrupted")
        && prompt.trim() != "/exit"
    {
        conversation.replace_runtime_context(Some(INTERRUPTED_RESUME_HINT.to_string()));
    }
    crate::skill_context::refresh_catalog(&mut conversation, cwd);
    conversation.add_user(prompt.to_string());
    crate::skill_context::append_mentions(&mut conversation, None, cwd, prompt)
        .expect("appending skill context without a writer cannot fail");
    conversation
}

pub(crate) fn bootstrap_agent_conversation_for_loop(
    cwd: &Path,
    prompt: &str,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    config: &RunConfig,
    memory: &MemoryBlock,
    allowed_tools: Option<&[String]>,
) -> Conversation {
    let system_prompt = agent_common::build_agent_system_prompt_sections(
        cwd,
        config,
        subagent_depth,
        subagent_type,
        Some(instructions),
        Some(memory),
        allowed_tools,
    );
    bootstrap_agent_conversation(None, system_prompt.into_message(), cwd, prompt)
}

pub(crate) fn record_plan_state_for_agent(
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    tool_request: &ToolRequest,
    result: &ToolResult,
) {
    if tool_request.name != ToolName::UpdatePlan || result.status != ToolStatus::Completed {
        return;
    }

    if let Ok(update) = orca_tools::update_plan::parse_args(tool_request) {
        conversation.replace_plan_state(orca_tools::update_plan::format_context_message(&update));
        if let Some(writer) = history_writer {
            let _ = writer.append_plan_state(update.explanation, update.plan);
        }
    }
}

impl InteractiveSession {
    /// Shared by hosted threads and the public session turn entry points.
    pub(crate) fn runtime_extensions(
        &mut self,
        id: &str,
    ) -> std::sync::Arc<crate::extension::ExtensionData> {
        self.runtime_extensions
            .get_or_insert_with(|| std::sync::Arc::new(crate::extension::ExtensionData::new(id)))
            .clone()
    }

    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
    ) -> io::Result<Self> {
        let mcp_registry =
            orca_mcp::initialize_registry(&config.mcp_servers, config.mcp_credentials_path.clone());
        mcp_registry.wait_for_startup(&|| false);
        Self::new_with_preloaded_and_mcp_registry(config, prompt_for_title, preloaded, mcp_registry)
    }

    pub fn new_with_preloaded_and_mcp_registry(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
    ) -> io::Result<Self> {
        Self::new_with_prepared_history(config, prompt_for_title, preloaded, mcp_registry, None)
    }

    pub(crate) fn new_with_prepared_history(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
        prepared_record_meta: Option<SessionMeta>,
    ) -> io::Result<Self> {
        Self::new_with_prepared_history_and_runtime_id(
            config,
            prompt_for_title,
            preloaded,
            mcp_registry,
            prepared_record_meta,
            None,
        )
    }

    pub(crate) fn new_with_prepared_history_and_runtime_id(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
        prepared_record_meta: Option<SessionMeta>,
        runtime_thread_id: Option<String>,
    ) -> io::Result<Self> {
        crate::pricing::refresh::ensure_background_refresh(config.pricing_refresh);
        // Like build_run_config (also used by detached workers), install the
        // user-config map for callers that supply a RunConfig directly.
        crate::pricing::book::set_user_overrides(crate::pricing::rates_from_overrides(
            &config.model.price_overrides(),
        ));
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let store = SessionStore::new();
        let instructions = instructions::load_for_cwd_or_default(&cwd);
        let memory = memory::load_for_cwd(&cwd);
        let hooks = HookRunner::new_with_capabilities(
            config.hooks.clone(),
            orca_core::capability::CapabilitySet::for_approval_mode(config.approval_mode),
        );
        let sections = agent_common::build_agent_system_prompt_sections(
            &cwd,
            config,
            0,
            &SubagentType::General,
            Some(&instructions),
            Some(&memory),
            None,
        );
        let system_prompt = sections.render();
        let current_prompt = sections.into_message();
        let (mut conversation, loaded_transcript) = match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => {
                let transcript = match preloaded {
                    Some(t) => t,
                    None => store.load_session(selector)?,
                };
                let mut conv =
                    crate::history::resume_conversation_in_cwd(&transcript, system_prompt, &cwd);
                conv.strip_legacy_pinned_volatile();
                conv.strip_legacy_summary_messages();
                conv.unpin_delivered_notices();
                crate::instructions::refresh_nested_instructions(&mut conv, &cwd);
                (conv, Some(transcript))
            }
            HistoryMode::ResumeAt {
                selector,
                resume_at,
            } => {
                let transcript = match preloaded {
                    Some(t) => t,
                    None => store.load_session(selector)?,
                };
                // Restore only the durable message boundary: records after the
                // requested conversation item id (including uncommitted tool
                // calls) are not replayed to the model.
                let transcript =
                    crate::thread_store::truncate_transcript_at_boundary(&transcript, resume_at)?;
                let mut conv =
                    crate::history::resume_conversation_in_cwd(&transcript, system_prompt, &cwd);
                conv.strip_legacy_pinned_volatile();
                conv.strip_legacy_summary_messages();
                conv.unpin_delivered_notices();
                crate::instructions::refresh_nested_instructions(&mut conv, &cwd);
                (conv, Some(transcript))
            }
            HistoryMode::Record | HistoryMode::Disabled => {
                let mut conversation = Conversation::new();
                conversation.messages.push(current_prompt.clone());
                (conversation, None)
            }
        };
        let lead = &mut conversation.messages[0];
        if matches!(config.history_mode, HistoryMode::Fork(_))
            && lead.system_prompt_record().is_none()
        {
            *lead = current_prompt.clone();
        }
        if loaded_transcript.is_some()
            && (lead.content_str() != current_prompt.content_str()
                || lead.system_prompt_record() != current_prompt.system_prompt_record())
        {
            conversation.compaction_system_prompt = Some(current_prompt.clone());
        }
        let resume_boundary = match &config.history_mode {
            HistoryMode::ResumeAt { resume_at, .. } => Some(resume_at.as_str()),
            _ => None,
        };
        let prompt_updates = crate::system_prompt::section_updates(
            &conversation.messages,
            &current_prompt,
            resume_boundary,
        );
        conversation.messages.extend(prompt_updates.iter().cloned());
        let usage_baseline = if matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::ResumeAt { .. }
        ) {
            loaded_transcript
                .as_ref()
                .and_then(|transcript| transcript.usage)
                .unwrap_or_default()
        } else {
            UsageTotals::default()
        };
        let mut session_id = None;
        let writer = match &config.history_mode {
            HistoryMode::Disabled => None,
            // Resume continues the original thread: keep its session id and
            // append future items to the existing transcript file. Only Fork
            // mints a new session id.
            HistoryMode::Resume(_) | HistoryMode::ResumeAt { .. } => match loaded_transcript {
                Some(transcript) => {
                    let thread_session_id = transcript.meta.session_id.clone();
                    match SessionWriter::append_to_existing(transcript.path) {
                        Ok(mut writer) => {
                            session_id = Some(thread_session_id);
                            for update in &prompt_updates {
                                if let Err(error) = writer.append_message(update) {
                                    eprintln!("orca: warning: history write failed: {error}");
                                    writer.mark_degraded();
                                    break;
                                }
                            }
                            Some(writer)
                        }
                        Err(error) => {
                            eprintln!("orca: warning: failed to reopen history: {error}");
                            None
                        }
                    }
                }
                None => None,
            },
            HistoryMode::Record => {
                if let Some(meta) = prepared_record_meta {
                    session_id = Some(meta.session_id.clone());
                    start_writer_with_messages(&store, meta, &conversation)
                } else {
                    match store.create_live_thread_with_permissions(
                        &cwd,
                        config.provider.as_str(),
                        config.model.as_history_value(),
                        prompt_for_title,
                        config.active_permission_profile.clone(),
                        config.approval_mode,
                        config.permission_rules.clone(),
                        config.additional_working_directories.clone(),
                    ) {
                        Ok(mut thread) => {
                            if let Err(error) = thread.append_items(&conversation.messages) {
                                eprintln!("orca: warning: history write failed: {error}");
                                None
                            } else {
                                let (thread_id, writer) = thread.into_thread_id_and_writer();
                                session_id = Some(thread_id);
                                Some(writer)
                            }
                        }
                        Err(error) => {
                            eprintln!("orca: warning: failed to initialize history: {error}");
                            None
                        }
                    }
                }
            }
            HistoryMode::Fork(_) => {
                let meta = prepared_record_meta.unwrap_or_else(|| {
                    let parent_id = loaded_transcript
                        .as_ref()
                        .map(|transcript| transcript.meta.session_id.clone())
                        .unwrap_or_default();
                    let mut meta = store.create_fork_meta(
                        &cwd,
                        config.provider.as_str(),
                        config.model.as_history_value(),
                        prompt_for_title,
                        parent_id,
                    );
                    meta.active_permission_profile = config.active_permission_profile.clone();
                    meta.approval_mode = Some(config.approval_mode);
                    meta.permission_rules = config.permission_rules.clone();
                    meta.additional_working_directories =
                        config.additional_working_directories.clone();
                    meta
                });
                let spill = crate::thread_store::spill::directory(&meta.session_id);
                if let Some(parent) = &meta.parent_id {
                    crate::thread_store::spill::carry_into_fork(
                        parent,
                        &meta.session_id,
                        &mut conversation,
                    );
                }
                session_id = Some(meta.session_id.clone());
                let writer = start_writer_with_messages(&store, meta, &conversation);
                if writer.is_none()
                    && let Err(error) = crate::thread_store::spill::remove(spill.as_deref())
                {
                    tracing::warn!(%error, "could not remove incomplete fork's saved tool results");
                }
                writer
            }
        };

        let mcp_registry = crate::mcp_startup::restore_tool_scope(mcp_registry, &conversation);

        let process_local_tasks = session_id.is_none() && runtime_thread_id.is_some();
        let task_session_id = session_id
            .clone()
            .or(runtime_thread_id)
            .unwrap_or_else(new_run_id);
        let task_registry = if process_local_tasks {
            TaskRegistry::new(task_session_id)
        } else if matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::ResumeAt { .. }
        ) {
            TaskRegistry::attach_for_cwd(task_session_id, &cwd)
        } else {
            TaskRegistry::new_for_cwd(task_session_id, &cwd)
        }
        .with_spill_directory(writer.as_ref().and_then(|writer| {
            writer
                .session_id()
                .as_deref()
                .and_then(crate::thread_store::spill::directory)
        }));

        let auto_memory_worker = (config.auto_memory
            && !matches!(config.history_mode, HistoryMode::Disabled))
        .then(memory::AutomaticMemoryWorker::start)
        .flatten();
        let cost_tracker = CostTracker::new(None);
        let session = Self {
            store,
            conversation,
            writer,
            session_id,
            completion_error: None,
            instructions,
            cost_tracker,
            usage_baseline,
            mcp_registry,
            hooks,
            memory,
            auto_memory_worker,
            turn_starts: HashMap::new(),
            completion_context: None,
            history_requested: !matches!(config.history_mode, HistoryMode::Disabled),
            task_registry,
            last_manual_compaction: None,
            runtime_extensions: None,
        };
        if let (Some(worker), Some(work)) = (
            session.auto_memory_worker.as_ref(),
            memory::automatic_memory_work_for_config(config, &cwd),
        ) {
            worker.wake(work);
        }
        Ok(session)
    }

    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn conversation_mut(&mut self) -> &mut Conversation {
        &mut self.conversation
    }

    pub(crate) fn take_manual_compaction_outcome(&mut self) -> Option<ManualCompactionOutcome> {
        self.last_manual_compaction.take()
    }

    pub(crate) fn manual_compaction_snapshot(
        &self,
        operation_id: &crate::runtime_surface::SurfaceOperationId,
    ) -> io::Result<Option<crate::thread_store::ManualCompactionDurableSnapshot>> {
        let Some(writer) = &self.writer else {
            return Ok(None);
        };
        crate::thread_store::read_manual_compaction_snapshot(writer.path(), operation_id)
    }

    pub fn writer_mut(&mut self) -> Option<&mut SessionWriter> {
        self.writer.as_mut()
    }

    /// Path of the live transcript, when this session is being recorded.
    pub(crate) fn writer_path(&self) -> Option<&Path> {
        self.writer.as_ref().map(SessionWriter::path)
    }

    /// Durable session id of the live transcript, when known.
    pub(crate) fn transcript_session_id(&self) -> Option<String> {
        self.writer.as_ref().and_then(SessionWriter::session_id)
    }

    /// Replaces the in-memory conversation wholesale (a conversation rewind
    /// rebuilds it from the truncated transcript).
    pub(crate) fn replace_conversation(&mut self, conversation: Conversation) {
        self.conversation = conversation;
        self.turn_starts.clear();
    }

    /// Lists the rewindable user messages with their checkpoint statistics.
    pub(crate) fn rewind_checkpoints(
        &self,
    ) -> io::Result<Vec<orca_core::rewind::RewindCheckpointView>> {
        crate::rewind::list_checkpoints(self)
    }

    /// Restores files and/or the conversation to the submission boundary of
    /// `target_message_id`. `dry_run` computes and returns the plan only.
    ///
    /// `config` supplies the provider settings the summarize modes use for
    /// their one summarizer request.
    pub(crate) fn rewind_to_checkpoint(
        &mut self,
        config: &RunConfig,
        target_message_id: &str,
        mode: orca_core::rewind::RewindMode,
        dry_run: bool,
    ) -> io::Result<orca_core::rewind::RewindOutcome> {
        crate::rewind::apply_rewind(self, config, target_message_id, mode, dry_run)
    }

    pub(crate) fn conversation_records(&self) -> Option<Vec<StoredConversationRecord>> {
        self.writer
            .as_ref()
            .map(SessionWriter::conversation_records)
    }

    pub(crate) fn history_requested(&self) -> bool {
        self.history_requested
    }

    #[cfg(test)]
    pub(crate) fn history_recording(&self) -> bool {
        self.writer.is_some()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn event_publication_store(&self) -> Option<(u64, SessionWriter)> {
        self.writer
            .as_ref()
            .cloned()
            .map(|writer| (writer.next_event_seq(), writer))
    }

    pub(crate) fn surface_commit_path(&self) -> Option<std::path::PathBuf> {
        self.writer
            .as_ref()
            .map(|writer| writer.path().to_path_buf())
    }

    pub fn completion_error(&self) -> Option<&str> {
        self.completion_error.as_deref()
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn instructions(&self) -> &ProjectInstructions {
        &self.instructions
    }

    pub fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    pub fn cost_tracker_mut(&mut self) -> &mut CostTracker {
        &mut self.cost_tracker
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.cost_tracker.totals()
    }

    pub fn aggregate_usage_totals(&self) -> UsageTotals {
        self.usage_baseline.plus(self.cost_tracker.totals())
    }

    pub fn mcp_registry(&self) -> &McpRegistry {
        &self.mcp_registry
    }

    pub fn hooks(&self) -> &HookRunner {
        &self.hooks
    }

    pub fn memory(&self) -> &MemoryBlock {
        &self.memory
    }

    pub(crate) fn enqueue_automatic_memory_turn(
        &mut self,
        config: &RunConfig,
        cwd: &Path,
        memory_start: usize,
        turn_id: &str,
        cancel: &orca_core::cancel::CancelToken,
    ) -> Result<(), String> {
        let Some(session_id) = self.session_id.as_deref() else {
            return Ok(());
        };
        let Some(worker) = self.auto_memory_worker.as_ref() else {
            return Ok(());
        };
        let messages = self.automatic_memory_turn_messages(memory_start, turn_id);
        let Some(work) = memory::enqueue_automatic_memory_turn(
            config, cwd, &messages, turn_id, session_id, cancel,
        )?
        else {
            return Ok(());
        };
        worker.wake(work);
        Ok(())
    }

    fn automatic_memory_turn_messages(&self, memory_start: usize, turn_id: &str) -> Vec<Message> {
        let durable_messages = self
            .writer
            .as_ref()
            .map(SessionWriter::conversation_records)
            .unwrap_or_default()
            .into_iter()
            .filter(|record| {
                record
                    .turn_id
                    .as_ref()
                    .is_some_and(|record_turn_id| record_turn_id.as_str() == turn_id)
            })
            .map(|record| Message::from(record.message))
            .collect::<Vec<_>>();
        if durable_messages
            .iter()
            .any(|message| matches!(message, Message::User { .. }))
        {
            durable_messages
        } else {
            self.conversation
                .messages
                .get(memory_start..)
                .unwrap_or_default()
                .to_vec()
        }
    }

    pub(crate) fn record_completion_context(&mut self, payload: &serde_json::Value) {
        let mut context = serde_json::Map::new();
        for key in [
            "resumable",
            "resume",
            "resume_unavailable_reason",
            "workspace_observation",
            "blockers",
        ] {
            if let Some(value) = payload.get(key) {
                context.insert(key.into(), value.clone());
            }
        }
        self.completion_context = Some(serde_json::Value::Object(context));
    }

    pub(crate) fn apply_completion_context(&self, payload: &mut serde_json::Value) {
        if let (Some(context), Some(payload)) = (
            self.completion_context.as_ref().and_then(|v| v.as_object()),
            payload.as_object_mut(),
        ) {
            for key in [
                "resumable",
                "resume",
                "resume_unavailable_reason",
                "workspace_observation",
                "blockers",
            ] {
                payload.remove(key);
            }
            payload.extend(context.clone());
        }
    }

    pub(crate) fn begin_runtime_turn(
        &mut self,
        turn_id: &str,
        existing_turn: bool,
        cwd: &Path,
        observe_workspace: bool,
    ) -> io::Result<TurnStart> {
        if existing_turn && let Some(start) = self.turn_starts.get(turn_id) {
            return Ok(start.clone());
        }
        self.completion_context = None;
        let start = TurnStart {
            workspace_baseline: if observe_workspace {
                crate::workspace_observation::Snapshot::capture(cwd)
            } else {
                Default::default()
            },
            message_index: self.conversation.messages.len(),
            file_write_baseline: self
                .task_registry
                .file_write_baseline()
                .map_err(io::Error::other)?,
        };
        // Keep one logical turn until another starts. A cancelled generation
        // may still be resumed by the host with this same turn identity.
        self.turn_starts.clear();
        self.turn_starts.insert(turn_id.to_string(), start.clone());
        Ok(start)
    }

    #[cfg(test)]
    pub(crate) fn wait_for_automatic_memory(&self) {
        self.wait_for_automatic_memory_snapshot();
    }

    pub(crate) fn wait_for_automatic_memory_snapshot(&self) {
        if let Some(worker) = self.auto_memory_worker.as_ref() {
            worker.barrier();
        }
    }

    pub(crate) fn set_task_registry(&mut self, registry: TaskRegistry) {
        self.task_registry = registry;
    }

    pub fn task_registry(&self) -> &TaskRegistry {
        &self.task_registry
    }

    pub(crate) fn runtime_parts(&mut self) -> InteractiveSessionRuntimeParts<'_> {
        InteractiveSessionRuntimeParts {
            conversation: &mut self.conversation,
            writer: self.writer.as_mut(),
            recorded: self.session_id.is_some(),
            instructions: &self.instructions,
            cost_tracker: &mut self.cost_tracker,
            mcp_registry: &self.mcp_registry,
            hooks: &self.hooks,
            memory: &self.memory,
            task_registry: &self.task_registry,
        }
    }

    pub fn has_active_workflows(&self) -> bool {
        self.task_registry.list().iter().any(|task| {
            matches!(
                task.status,
                TaskStatus::Queued
                    | TaskStatus::Running
                    | TaskStatus::Paused
                    | TaskStatus::Stopping
            )
        })
    }

    pub fn append_message(&mut self, message: &orca_core::conversation::Message) {
        if let Some(writer) = &mut self.writer {
            if let Err(error) = writer.append_detached_message(message) {
                eprintln!("orca: warning: history write failed: {error}");
                self.writer = None;
            }
        }
    }

    pub fn complete(&mut self, status: &str) {
        self.complete_with_error(status, None);
    }

    pub fn complete_with_error(&mut self, status: &str, error: Option<&str>) {
        let _ = self.complete_with_error_durable(status, error);
    }

    pub(crate) fn complete_with_error_durable(
        &mut self,
        status: &str,
        error: Option<&str>,
    ) -> bool {
        self.completion_error = error.map(str::to_string);
        let Some(writer) = &mut self.writer else {
            return false;
        };
        match writer.complete_with_error(status, error) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("orca: warning: history completion write failed: {error}");
                false
            }
        }
    }

    /// Complete the transcript from the typed operation terminal. The status
    /// string is derived from the terminal object, never invented by the
    /// projection.
    pub fn complete_with_terminal(&mut self, terminal: &orca_core::budget::OperationTerminal) {
        let _ = self.complete_with_terminal_durable(terminal);
    }

    pub(crate) fn complete_with_terminal_durable(
        &mut self,
        terminal: &orca_core::budget::OperationTerminal,
    ) -> bool {
        let status = terminal.as_str();
        let error = match terminal {
            orca_core::budget::OperationTerminal::Failed { message, .. } => Some(message.as_str()),
            _ => None,
        };
        self.complete_with_error_durable(status, error)
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        self.conversation.backtrack_last_user()
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.cost_tracker.set_model(model);
    }

    pub fn add_pinned_context(&mut self, content: String) {
        self.conversation.add_user_pinned(content);
        if let Some(message) = self.conversation.messages.last().cloned() {
            self.append_message(&message);
        }
    }

    pub fn replace_goal_context(&mut self, content: Option<String>) {
        self.conversation.replace_goal_state(content);
    }

    pub fn replace_skill_context(&mut self, content: Option<String>) {
        self.conversation.replace_skill_context(content);
    }

    pub(crate) fn compact<F>(
        &mut self,
        config: &RunConfig,
        cwd: &Path,
        session_tools: &crate::extension::ExtensionData,
        cancel: &orca_core::cancel::CancelToken,
        precommit: F,
    ) -> io::Result<ManualCompactionOutcome>
    where
        F: FnOnce(&ManualCompactionOutcome) -> io::Result<ManualCompactionPersistenceIdentity>,
    {
        let before = self.conversation.clone();
        let before_messages = self.conversation.messages.len();
        let mut candidate = self.conversation.clone();
        if let Ok(outcome) = self.hooks.run_with_cancel(
            HookEvent::OnBudgetWarning,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
            cancel,
        ) && !outcome.injected_context.is_empty()
        {
            candidate = conversation_with_hook_context(&candidate, &outcome);
        }
        let _ = self.hooks.run_with_cancel(
            HookEvent::PreCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
            cancel,
        );
        let mut provider_config = crate::tool_invocation::provider_config_for_agent_loop(
            config,
            cwd,
            0,
            &SubagentType::General,
            crate::tool_invocation::AgentToolPolicyContext::unrestricted(),
            &self.mcp_registry,
            session_tools,
        );
        let model = config.model.route(orca_core::model::ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
            has_images: false,
        });
        provider_config.model = Some(model.actual_model);
        let mut recorded = false;
        let compaction = orca_provider::context::compact_with_summary_settled(
            config.provider,
            &candidate,
            &orca_provider::context::ContextConfig::for_model_with_runtime(
                provider_config.model.as_deref(),
                &config.model_runtime,
                config.reasoning_effort,
            ),
            &provider_config,
            Some(cancel),
            false,
            &mut |model, usage| {
                if let Some(usage) = usage.filter(|usage| !usage.is_empty()) {
                    self.cost_tracker.add_model_usage(model, usage);
                    recorded = true;
                }
                config.budget.max_cost_usd_micros.is_none_or(|limit| {
                    crate::cost::usd_to_micros(self.cost_tracker.totals().estimated_cost_usd)
                        <= limit
                })
            },
        );
        let strategy = match &compaction.kind {
            orca_provider::context::CompactionKind::LocalTruncation => "local_truncation",
            orca_provider::context::CompactionKind::RemoteSummary(_) => "remote_summary",
        };
        let usage = recorded.then(|| self.cost_tracker.totals());
        if let (Some(writer), Some(totals)) = (&mut self.writer, usage) {
            writer.append_usage(totals)?;
        }
        let after = compaction.conversation;
        let after_messages = after.messages.len();
        let outcome = ManualCompactionOutcome {
            before,
            after,
            strategy,
            observation: compaction.observation,
            usage,
        };
        let identity = precommit(&outcome)?;
        if let Some(writer) = &mut self.writer {
            writer.append_manual_compaction_snapshot(
                &identity,
                before_messages,
                outcome.strategy,
                &outcome.after,
            )?;
        }
        self.conversation.adopt(outcome.after.clone());
        self.last_manual_compaction = Some(outcome.clone());
        let _ = self.hooks.run_with_cancel(
            HookEvent::PostCompact,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: Some(after_messages),
                usage: None,
            },
            cancel,
        );
        Ok(outcome)
    }
}

fn start_writer_with_messages(
    store: &SessionStore,
    meta: SessionMeta,
    conversation: &Conversation,
) -> Option<SessionWriter> {
    match store.start_writer_from_meta(meta) {
        Ok(mut writer) => {
            for message in &conversation.messages {
                if let Err(error) = writer.append_legacy_message(message) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            for version in conversation.nested_instructions.versions.values() {
                if let Err(error) = writer.append_nested_instruction(version) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            if !conversation.summary.is_empty() {
                let inherited_marker = conversation
                    .summary
                    .latest_rolling()
                    .map(|text| text.to_string())
                    .unwrap_or_default();
                let count = conversation.messages.len();
                if let Err(error) = writer.append_summary_state(
                    count,
                    count,
                    inherited_marker,
                    &conversation.summary,
                ) {
                    eprintln!("orca: warning: history write failed: {error}");
                    return None;
                }
            }
            Some(writer)
        }
        Err(error) => {
            eprintln!("orca: warning: failed to initialize history: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::task_types::TaskStatus;
    use orca_core::thread_identity::TurnId;
    use orca_provider::prompt_cache::PromptCacheCheckpoint;
    use tempfile::tempdir;

    use super::*;
    use crate::history;

    fn config(cwd: PathBuf, history_mode: HistoryMode) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Text,
            execution_profile: orca_core::capability::ExecutionProfile::Workspace,
            approval_mode: ApprovalMode::Suggest,
            startup_approval_mode: None,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            mcp_credentials_path: None,
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            subagents: SubagentConfig::default(),
            computer_use: orca_core::config::ComputerUseConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            pricing_refresh: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        // Provide an exclusive never-removed subdirectory of the process-wide
        // isolated home for repository/trust fixtures, and resolve ORCA_HOME
        // to it for this thread (a thread-local override; the environment is
        // never mutated), so concurrent tests always resolve the process-wide
        // home.
        let _guard = history::lock_test_env();
        let home = history::isolated_test_orca_home_subdir("with-orca-home");
        history::with_test_orca_home(&home, f)
    }

    #[test]
    fn resume_rebuilds_the_last_appended_state_without_new_messages() {
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let mut writer =
                SessionWriter::start(cwd.path(), "deepseek", None, "read records").unwrap();
            writer
                .append_message(&Message::system("system prompt".into()))
                .unwrap();
            writer.enter_turn(TurnId::new());
            writer
                .append_message(&Message::user("read records".into()))
                .unwrap();
            let content = "Current memory context (this replaces any earlier version):\n[Relevant memory]\nfirst fact";
            let mut original = Conversation::new();
            original.replace_memory_context(Some("first fact".into()));
            let batch = orca_provider::context::pending_context_message(&original).unwrap();
            writer.append_message(&batch).unwrap();
            let transcript = history::load_session("latest").unwrap();
            let mut resumed = history::resume_conversation(&transcript, "system prompt".into());
            assert_eq!(resumed.messages.len(), 3);
            assert_eq!(resumed.messages[2].content_str(), Some(content));
            resumed.replace_memory_context(Some("first fact".into()));
            assert!(orca_provider::context::pending_context_message(&resumed).is_none());
        });
    }

    fn record_prompt_session(cwd: &Path) -> (std::path::PathBuf, String, SessionTranscript) {
        let mut session = InteractiveSession::new_with_preloaded_and_mcp_registry(
            &config(cwd.into(), HistoryMode::Record),
            "read records",
            None,
            McpRegistry::default(),
        )
        .unwrap();
        let prompt = session.conversation.messages[0]
            .content_str()
            .unwrap()
            .to_owned();
        let writer = session.writer_mut().unwrap();
        writer.enter_turn(TurnId::new());
        writer
            .append_message(&Message::user("read records".into()))
            .unwrap();
        let path = writer.path().to_path_buf();
        let transcript = history::load_session("latest").unwrap();
        (path, prompt, transcript)
    }

    fn reopen_prompt_session(
        cwd: &Path,
        mode: HistoryMode,
        transcript: SessionTranscript,
    ) -> InteractiveSession {
        InteractiveSession::new_with_preloaded_and_mcp_registry(
            &config(cwd.into(), mode),
            "continue",
            Some(transcript),
            McpRegistry::default(),
        )
        .unwrap()
    }

    fn prompt_section_notices(session: &InteractiveSession) -> Vec<&str> {
        session
            .conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content, .. } if content.starts_with("The \"") => {
                    Some(content.as_str())
                }
                _ => None,
            })
            .collect()
    }

    fn prompt_wire_record(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .find(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["message"]["role"]
                    == "system"
            })
            .unwrap()
            .to_string()
    }

    #[test]
    fn the_system_prompt_sections_are_recorded_at_session_start() {
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let (path, prompt, _) = record_prompt_session(cwd.path());
            let record: serde_json::Value =
                serde_json::from_str(&prompt_wire_record(&path)).unwrap();
            let sections = record["message"]["system_prompt"]["sections"]
                .as_array()
                .expect("recorded sections");
            assert!(sections.iter().any(|section| {
                section["name"] == "cwd"
                    && section["text"]
                        .as_str()
                        .unwrap()
                        .contains(&cwd.path().display().to_string())
            }));
            let reconstructed = sections
                .iter()
                .map(|section| section["text"].as_str().unwrap())
                .collect::<String>();
            assert_eq!(reconstructed, prompt);
        });
    }

    #[test]
    fn resume_with_an_unchanged_prompt_sends_the_recorded_prompt() {
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let (path, prompt, transcript) = record_prompt_session(cwd.path());
            let id = transcript.meta.session_id.clone();
            let resumed = reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), transcript);
            assert_eq!(
                resumed.conversation.messages[0].content_str(),
                Some(prompt.as_str())
            );
            assert!(prompt_section_notices(&resumed).is_empty());
            let record: serde_json::Value =
                serde_json::from_str(&prompt_wire_record(&path)).unwrap();
            assert!(record["message"]["system_prompt"]["sections"].is_array());
        });
    }

    #[test]
    fn resume_after_a_prompt_change_appends_one_notice() {
        with_orca_home(|orca_home| {
            let cwd = tempdir().unwrap();
            let agents = orca_home.join("AGENTS.md");
            std::fs::write(&agents, "Read the local runbook.").unwrap();
            let (_, prompt, transcript) = record_prompt_session(cwd.path());
            let id = transcript.meta.session_id.clone();
            let mut original = Conversation::new();
            original.add_system(prompt.clone());
            original.add_user("read records".into());
            std::fs::write(&agents, "Use the new local runbook.").unwrap();
            let resumed =
                reopen_prompt_session(cwd.path(), HistoryMode::Resume(id.clone()), transcript);
            assert_eq!(prompt_section_notices(&resumed).len(), 1);
            assert_eq!(
                resumed.conversation.messages[0].content_str(),
                Some(prompt.as_str())
            );
            let notice = prompt_section_notices(&resumed)[0];
            assert!(notice.starts_with(
                "The \"project instructions\" instructions changed; they now read:\n"
            ));
            assert!(notice.contains("Use the new local runbook."));
            let provider = orca_provider::ProviderConfig {
                api_key: None,
                base_url: None,
                model: None,
                reasoning_effort: Default::default(),
                tools_override: Some(Vec::new()),
                mcp_registry: None,
                external_tools: Vec::new(),
                max_output_tokens: None,
            };
            let prior =
                orca_provider::prompt_cache::checkpoint_for_deepseek_request(&original, &provider)
                    .unwrap();
            assert!(
                prior
                    .matches_deepseek_prefix(&resumed.conversation, &provider)
                    .unwrap()
            );
            drop(resumed);
            let reopened = reopen_prompt_session(
                cwd.path(),
                HistoryMode::Resume(id),
                history::load_session("latest").unwrap(),
            );
            assert_eq!(prompt_section_notices(&reopened).len(), 1);
        });
    }

    #[test]
    fn revoked_trust_rebuilds_the_prompt_without_the_old_instructions() {
        use orca_core::config::folder_trust::{TrustLevel, set_trust};

        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            std::fs::create_dir(cwd.path().join(".git")).unwrap();
            std::fs::write(cwd.path().join("AGENTS.md"), "Follow the deploy runbook.").unwrap();
            set_trust(cwd.path(), TrustLevel::Trusted).unwrap();
            let (_, prompt, transcript) = record_prompt_session(cwd.path());
            assert!(prompt.contains("Follow the deploy runbook."));
            set_trust(cwd.path(), TrustLevel::Untrusted).unwrap();

            let id = transcript.meta.session_id.clone();
            for mode in [
                HistoryMode::Resume(id.clone()),
                HistoryMode::Fork(id.clone()),
            ] {
                let session = reopen_prompt_session(cwd.path(), mode, transcript.clone());
                assert!(prompt_section_notices(&session).is_empty());
                assert!(session.conversation.messages.iter().all(|message| {
                    !message
                        .content_str()
                        .unwrap_or_default()
                        .contains("Follow the deploy runbook.")
                }));
            }
        });
    }

    #[test]
    fn revoked_nested_trust_rebuilds_the_prompt_without_the_delivered_instructions() {
        use orca_core::config::folder_trust::{TrustLevel, set_trust};

        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            let child = cwd.path().join("src");
            std::fs::create_dir(&child).unwrap();
            let touched = child.join("one.txt");
            std::fs::write(&touched, "fixture").unwrap();
            std::fs::write(child.join("AGENTS.md"), "Use the scoped runbook.").unwrap();
            std::fs::write(home.join("AGENTS.md"), "Use the old global runbook.").unwrap();
            set_trust(cwd.path(), TrustLevel::Trusted).unwrap();
            let (path, _, transcript) = record_prompt_session(cwd.path());
            let mut writer = SessionWriter::append_to_existing(path).unwrap();
            let mut conversation = Conversation::new();
            conversation
                .nested_instructions
                .pending_touches
                .push((touched, false));
            instructions::load_for_cwd(cwd.path())
                .unwrap()
                .append_nested_for_touches(&mut conversation, Some(&mut writer));
            let batch = orca_provider::context::pending_context_message(&conversation).unwrap();
            assert!(
                batch
                    .content_str()
                    .unwrap()
                    .contains("Use the scoped runbook.")
            );
            writer.append_message(&batch).unwrap();
            drop(writer);

            let id = transcript.meta.session_id;
            let transcript = history::load_session(&id).unwrap();
            set_trust(&child, TrustLevel::Untrusted).unwrap();
            std::fs::write(home.join("AGENTS.md"), "Use the live global runbook.").unwrap();
            for mode in [
                HistoryMode::Resume(id.clone()),
                HistoryMode::Fork(id.clone()),
            ] {
                let session = reopen_prompt_session(cwd.path(), mode, transcript.clone());
                assert!(
                    session.conversation.messages[0]
                        .content_str()
                        .unwrap()
                        .contains("Use the live global runbook.")
                );
                assert!(prompt_section_notices(&session).is_empty());
                assert!(session.conversation.messages.iter().all(|message| {
                    !message
                        .content_str()
                        .unwrap_or_default()
                        .contains("Use the scoped runbook.")
                }));
            }
        });
    }

    #[test]
    fn redaction_does_not_change_the_sent_prompt_or_fragments() {
        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            std::fs::write(home.join("AGENTS.md"), "Token: required").unwrap();
            let (path, prompt, transcript) = record_prompt_session(cwd.path());
            let mut session = Conversation::new();
            session.replace_runtime_context(Some("retained runtime hint".into()));
            session.replace_memory_context(Some("Token: optional".into()));
            let batch = orca_provider::context::pending_context_message(&session).unwrap();
            assert!(prompt.contains("Token: required"));
            assert!(batch.content_str().unwrap().contains("Token: optional"));
            let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
            writer.append_message(&batch).unwrap();
            drop(writer);
            let raw = std::fs::read_to_string(path).unwrap();
            assert!(raw.contains("Token: <redacted>"));
            assert!(!raw.contains("Token: required") && !raw.contains("Token: optional"));
            let transcript = history::load_session(&transcript.meta.session_id).unwrap();
            let Message::System {
                context_fragments, ..
            } = transcript.messages.last().unwrap()
            else {
                panic!("expected saved context batch")
            };
            assert!(
                context_fragments
                    .iter()
                    .find(|change| change.id == "memory")
                    .unwrap()
                    .digest
                    .is_none()
            );
            assert!(
                context_fragments
                    .iter()
                    .find(|change| change.id == "runtime")
                    .unwrap()
                    .digest
                    .is_some()
            );
            let mut resumed = history::resume_conversation(&transcript, prompt);
            resumed.replace_runtime_context(Some("retained runtime hint".into()));
            resumed.replace_memory_context(Some("Token: optional".into()));
            let pending = orca_provider::context::pending_context_message(&resumed).unwrap();
            assert!(pending.content_str().unwrap().contains("Token: optional"));
            assert!(
                !pending
                    .content_str()
                    .unwrap()
                    .contains("retained runtime hint")
            );
            assert!(resumed.messages.iter().all(|message| {
                !message
                    .content_str()
                    .unwrap_or_default()
                    .contains("Token: <redacted>")
            }));
            resumed.messages.push(pending);
            assert!(orca_provider::context::pending_context_message(&resumed).is_none());
        });
    }

    #[test]
    fn a_change_hidden_by_redaction_is_still_delivered() {
        let mut conversation = Conversation::new();
        conversation.replace_memory_context(Some("Token: required".into()));
        conversation
            .messages
            .extend(orca_provider::context::pending_context_message(
                &conversation,
            ));
        conversation.replace_memory_context(Some("Token: optional".into()));
        let update = orca_provider::context::pending_context_message(&conversation)
            .expect("changed fragment");
        assert!(update.content_str().unwrap().contains("Token: optional"));
        conversation.messages.push(update);
        assert!(orca_provider::context::pending_context_message(&conversation).is_none());
    }

    #[test]
    fn a_redacted_section_resumes_by_rebuilding() {
        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            let source = home.join("AGENTS.md");
            std::fs::write(&source, "Token: required").unwrap();
            let (_, _, transcript) = record_prompt_session(cwd.path());
            std::fs::write(source, "Token: optional").unwrap();
            let id = transcript.meta.session_id.clone();
            let session = reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), transcript);
            assert!(
                session.conversation.messages[0]
                    .content_str()
                    .unwrap()
                    .contains("Token: optional")
            );
            assert!(prompt_section_notices(&session).is_empty());
        });
    }

    #[test]
    fn resuming_twice_at_the_same_boundary_writes_one_notice() {
        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            let agents = home.join("AGENTS.md");
            std::fs::write(&agents, "Use the original runbook.").unwrap();
            let (path, _, transcript) = record_prompt_session(cwd.path());
            let boundary = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .find_map(|line| {
                    let record: serde_json::Value = serde_json::from_str(line).unwrap();
                    (record["message"]["role"] == "user")
                        .then(|| record["id"].as_str().unwrap().to_owned())
                })
                .unwrap();
            let id = transcript.meta.session_id;
            std::fs::write(&agents, "Use the replacement runbook.").unwrap();
            for _ in 0..2 {
                let resumed = reopen_prompt_session(
                    cwd.path(),
                    HistoryMode::ResumeAt {
                        selector: id.clone(),
                        resume_at: boundary.clone(),
                    },
                    history::load_session(&id).unwrap(),
                );
                assert_eq!(prompt_section_notices(&resumed).len(), 1);
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                raw.lines()
                    .filter(|line| line.contains("Use the replacement runbook."))
                    .count(),
                1
            );
            let resumed = reopen_prompt_session(
                cwd.path(),
                HistoryMode::Resume(id.clone()),
                history::load_session(&id).unwrap(),
            );
            assert_eq!(prompt_section_notices(&resumed).len(), 1);
        });
    }

    #[test]
    fn a_removed_section_appends_the_no_longer_applies_note() {
        with_orca_home(|orca_home| {
            let cwd = tempdir().unwrap();
            let agents = orca_home.join("AGENTS.md");
            std::fs::write(&agents, "Use the old local runbook.").unwrap();
            let (_, _, transcript) = record_prompt_session(cwd.path());
            std::fs::remove_file(&agents).unwrap();
            let id = transcript.meta.session_id.clone();
            let resumed = reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), transcript);
            assert_eq!(
                prompt_section_notices(&resumed),
                ["The \"project instructions\" instructions no longer apply."]
            );
        });
    }

    #[test]
    fn fork_reports_the_cwd_and_project_instruction_paths() {
        use orca_core::config::folder_trust::{TrustLevel, set_trust};

        with_orca_home(|_| {
            let fixture = tempdir().unwrap();
            let parent = fixture.path().join("source");
            let child = fixture.path().join("worktree");
            std::fs::create_dir(&parent).unwrap();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .current_dir(&parent)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init"]);
            std::fs::write(parent.join("AGENTS.md"), "Use the repository runbook.").unwrap();
            git(&["add", "AGENTS.md"]);
            git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Add fixture instructions",
            ]);
            git(&[
                "worktree",
                "add",
                "--detach",
                child.to_str().unwrap(),
                "HEAD",
            ]);
            assert_eq!(
                std::fs::read(parent.join("AGENTS.md")).unwrap(),
                std::fs::read(child.join("AGENTS.md")).unwrap()
            );
            set_trust(&parent, TrustLevel::Trusted).unwrap();
            set_trust(&child, TrustLevel::Trusted).unwrap();
            let (_, prompt, transcript) = record_prompt_session(&parent);
            let parent_id = transcript.meta.session_id.clone();
            let fork = reopen_prompt_session(&child, HistoryMode::Fork(parent_id), transcript);
            assert_eq!(
                fork.conversation.messages[0].content_str(),
                Some(prompt.as_str())
            );
            assert_eq!(
                prompt_section_notices(&fork),
                [
                    format!(
                        "The \"cwd\" instructions changed; they now read:\n- Working directory: {}",
                        child.display()
                    ),
                    format!(
                        "The \"project instructions\" instructions changed; they now read:\n<project-instructions>\n# {}\nUse the repository runbook.\n</project-instructions>",
                        child.join("AGENTS.md").display()
                    )
                ]
            );
            let fork_id = fork.session_id().unwrap().to_owned();
            let persisted = history::load_session(&fork_id).unwrap();
            let resumed = reopen_prompt_session(&child, HistoryMode::Resume(fork_id), persisted);
            assert_eq!(prompt_section_notices(&resumed).len(), 2);
        });
    }

    fn resumes_with_a_rebuilt_prompt(edit: impl FnOnce(&mut serde_json::Value)) {
        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            let agents = home.join("AGENTS.md");
            std::fs::write(&agents, "Use yesterday's runbook.").unwrap();
            let (path, _, transcript) = record_prompt_session(cwd.path());
            let original = prompt_wire_record(&path);
            let mut record: serde_json::Value = serde_json::from_str(&original).unwrap();
            edit(&mut record["message"]);
            let raw =
                std::fs::read_to_string(&path)
                    .unwrap()
                    .replacen(&original, &record.to_string(), 1);
            std::fs::write(&path, raw).unwrap();
            std::fs::write(&agents, "Use today's runbook.").unwrap();
            let id = transcript.meta.session_id;
            let loaded =
                history::load_session(&id).expect("optional metadata must leave history readable");
            let session = reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), loaded);
            let prompt = session.conversation.messages[0].content_str().unwrap();
            assert!(prompt.contains("Use today's runbook."));
            assert!(prompt_section_notices(&session).is_empty());
        });
    }

    #[test]
    fn duplicate_prompt_section_names_rebuild_the_prompt() {
        resumes_with_a_rebuilt_prompt(|message| {
            message["system_prompt"]["sections"][1]["name"] =
                message["system_prompt"]["sections"][0]["name"].clone();
        });
    }

    #[test]
    fn prompt_sections_must_reconstruct_the_stored_content() {
        resumes_with_a_rebuilt_prompt(|message| {
            message["content"] = "A different prompt".into();
        });
    }

    #[test]
    fn malformed_prompt_sections_leave_the_transcript_readable() {
        resumes_with_a_rebuilt_prompt(|message| {
            message["system_prompt"]["sections"] = serde_json::json!({});
        });
    }

    #[test]
    fn an_unknown_prompt_metadata_version_rebuilds_without_notices() {
        resumes_with_a_rebuilt_prompt(|message| {
            message["system_prompt"]["version"] = 999.into();
            message["system_prompt"]["sections"][0]["name"] = "future section".into();
        });
    }

    #[test]
    fn a_session_without_recorded_sections_resumes_as_before() {
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let (path, current, _) = record_prompt_session(cwd.path());
            let fresh: serde_json::Value =
                serde_json::from_str(&prompt_wire_record(&path)).unwrap();
            assert!(fresh["message"]["system_prompt"]["sections"].is_array());
            let mut writer = SessionWriter::start(cwd.path(), "mock", None, "legacy").unwrap();
            writer
                .append_message(&Message::system("old unsectioned prompt".into()))
                .unwrap();
            let transcript = history::load_session("latest").unwrap();
            let id = transcript.meta.session_id.clone();
            let resumed = reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), transcript);
            assert_eq!(
                resumed.conversation.messages[0].content_str(),
                Some(current.as_str())
            );
            assert!(prompt_section_notices(&resumed).is_empty());
        });
    }

    #[test]
    fn the_recorded_sections_are_readable_by_the_previous_release() {
        // System-message arms from the v0.5.25 SessionRecord and StoredMessageWire.
        #[derive(serde::Deserialize)]
        #[serde(tag = "type")]
        enum PreviousSessionRecord {
            #[serde(rename = "conversation.message")]
            Message {
                id: Option<orca_core::thread_identity::ConversationItemId>,
                turn_id: Option<TurnId>,
                message: PreviousStoredMessage,
            },
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case", tag = "role")]
        enum PreviousStoredMessage {
            System {
                content: String,
                #[serde(default)]
                pinned: bool,
            },
        }
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let (path, prompt, _) = record_prompt_session(cwd.path());
            let line = prompt_wire_record(&path);
            let record: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(record["message"]["system_prompt"]["sections"].is_array());
            let PreviousSessionRecord::Message {
                id,
                turn_id,
                message: PreviousStoredMessage::System { content, pinned },
            } = serde_json::from_str(&line).unwrap();
            assert_eq!(content, prompt);
            assert!(!pinned);
            assert!(id.is_none());
            assert!(turn_id.is_none());
        });
    }

    #[test]
    fn compaction_rebuilds_the_changed_system_prompt() {
        with_orca_home(|orca_home| {
            let cwd = tempdir().unwrap();
            let agents = orca_home.join("AGENTS.md");
            std::fs::write(&agents, "Use the old local runbook.").unwrap();
            let (_, _, transcript) = record_prompt_session(cwd.path());
            std::fs::write(&agents, "Use the new local runbook.").unwrap();
            let id = transcript.meta.session_id.clone();
            let mut resumed =
                reopen_prompt_session(cwd.path(), HistoryMode::Resume(id), transcript);
            assert!(
                resumed.conversation.messages[0]
                    .content_str()
                    .unwrap()
                    .contains("Use the old local runbook.")
            );
            resumed
                .conversation
                .add_assistant(Some("old answer ".repeat(5000)), None, vec![]);
            resumed.conversation.add_user("continue".into());
            let compacted = orca_provider::context::compact(
                &resumed.conversation,
                &orca_provider::context::ContextConfig {
                    max_tokens: 4000,
                    compaction_threshold: 1.0,
                    reserved_for_response: 0,
                    auto_compact_token_limit: None,
                    soft_compact_token_limit: None,
                },
            );
            assert!(
                compacted.messages[0]
                    .content_str()
                    .unwrap()
                    .contains("Use the new local runbook.")
            );
            assert!(
                !compacted
                    .messages
                    .iter()
                    .any(|message| message.content_str().is_some_and(|text| text
                        .starts_with("The \"project instructions\" instructions changed")))
            );
        });
    }

    #[test]
    fn session_from_handmade_run_config_installs_user_price_overrides() {
        crate::pricing::tests::with_isolated_user_overrides(|| {
            use orca_core::model::{ModelDefinition, ModelPriceOverride, UsdPerMillion};

            let cwd = tempdir().unwrap();
            let model = "handmade-session-user-price-probe";
            let mut cfg = config(cwd.path().into(), HistoryMode::Disabled);
            cfg.model = ModelSelection::parse_with_models(
                Some(model.into()),
                std::collections::BTreeMap::from([(
                    model.into(),
                    ModelDefinition {
                        pricing: Some(ModelPriceOverride {
                            input_cache_miss: UsdPerMillion::new(1.0).unwrap(),
                            input_cache_hit: UsdPerMillion::new(0.5).unwrap(),
                            output: UsdPerMillion::new(2.0).unwrap(),
                        }),
                        ..ModelDefinition::default()
                    },
                )]),
            )
            .unwrap();
            assert!(!crate::pricing::book::user_overrides().contains_key(model));

            let _session = InteractiveSession::new_with_preloaded(&cfg, "price probe", None)
                .expect("session from a hand-made RunConfig");

            assert_eq!(
                crate::pricing::book::user_overrides().get(model),
                Some(&crate::pricing::rates::Rates {
                    input_cache_miss: 1.0,
                    input_cache_hit: 0.5,
                    output: 2.0,
                })
            );
        });
    }

    #[test]
    fn headless_main_session_prompt_does_not_advertise_computer() {
        with_computer_prompt_home(|home| {
            let mut cfg = config(home.to_path_buf(), HistoryMode::Disabled);
            let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
                &cfg,
                "headless",
                None,
                McpRegistry::default(),
            )
            .expect("session");
            let prompt = session.conversation.messages[0].content_str().unwrap();
            assert!(prompt.contains("`read_file`, `glob`, and `grep`"));
            assert!(!prompt.contains("### computer"));
            cfg.computer_use.interactive = true;
            let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
                &cfg,
                "interactive",
                None,
                McpRegistry::default(),
            )
            .expect("interactive session");
            let prompt = session.conversation.messages[0].content_str().unwrap();
            assert!(!prompt.contains("### computer"));
        });
    }

    #[test]
    fn denied_main_session_prompt_does_not_advertise_computer() {
        with_computer_prompt_home(|home| {
            let mut cfg = config(home.to_path_buf(), HistoryMode::Disabled);
            cfg.computer_use.interactive = true;
            cfg.permission_rules
                .rules
                .push(orca_core::approval_rules::PermissionRule::whole_tool(
                    "computer",
                    orca_core::approval_types::Decision::Deny,
                ));
            let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
                &cfg,
                "denied",
                None,
                McpRegistry::default(),
            )
            .expect("session");
            let prompt = session.conversation.messages[0].content_str().unwrap();
            assert!(prompt.contains("`read_file`, `glob`, and `grep`"));
            assert!(!prompt.contains("### computer"));
        });
    }

    #[test]
    fn custom_main_session_prompt_does_not_advertise_computer() {
        with_computer_prompt_home(|home| {
            let mut cfg = config(home.to_path_buf(), HistoryMode::Disabled);
            cfg.computer_use.interactive = true;
            cfg.subagents.effective_definition = Some(
                orca_core::subagent_types::agent_definition::EffectiveAgentDefinition {
                    name: "desktop-audit".to_string(),
                    description: "Audit source".to_string(),
                    system_prompt: "Report source findings.".to_string(),
                    allowed_tools: vec!["computer".to_string(), "read_file".to_string()],
                    model: None,
                },
            );
            let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
                &cfg,
                "custom",
                None,
                McpRegistry::default(),
            )
            .expect("session");
            let prompt = session.conversation.messages[0].content_str().unwrap();
            assert!(prompt.contains("`read_file`, `glob`, and `grep`"));
            assert!(!prompt.contains("### computer"));
            assert!(!prompt.contains("### bash"));
            assert!(prompt.contains("Report source findings."));
        });
    }

    fn with_computer_prompt_home(f: impl FnOnce(&Path)) {
        with_orca_home(f);
    }

    #[test]
    fn automatic_memory_recovers_the_exact_turn_from_the_durable_ledger() {
        with_orca_home(|home| {
            let mut cfg = config(home.to_path_buf(), HistoryMode::Record);
            cfg.auto_memory = true;
            let mut session = InteractiveSession::new_with_preloaded(&cfg, "first prompt", None)
                .expect("session");
            let earlier_turn = TurnId::new();
            let current_turn = TurnId::new();
            let writer = session.writer_mut().expect("writer");
            writer.enter_turn(earlier_turn);
            writer
                .append_message(&Message::user("stale historical prompt".to_string()))
                .expect("earlier prompt");
            writer.enter_turn(current_turn.clone());
            writer
                .append_message(&Message::user("current durable prompt".to_string()))
                .expect("current prompt");
            writer
                .append_message(&Message::Assistant {
                    cut_short: None,
                    content: Some("current durable response".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                })
                .expect("current response");

            let messages = session.automatic_memory_turn_messages(
                session.conversation.messages.len(),
                current_turn.as_str(),
            );
            let contents = messages
                .iter()
                .filter_map(Message::content_str)
                .collect::<Vec<_>>();

            assert!(contents.contains(&"current durable prompt"));
            assert!(contents.contains(&"current durable response"));
            assert!(!contents.contains(&"stale historical prompt"));
        });
    }

    #[test]
    fn a_fork_keeps_its_own_copy_of_inherited_large_tool_results() {
        use orca_core::conversation::CutShort;

        with_orca_home(|home| {
            let cwd = tempdir().unwrap();
            let mut parent =
                history::SessionWriter::start(cwd.path(), "deepseek", None, "parent").unwrap();
            let parent_id = parent.session_id().unwrap();
            let parent_dir = crate::thread_store::spill::directory(&parent_id).unwrap();
            assert!(parent_dir.starts_with(home));
            std::fs::create_dir_all(&parent_dir).unwrap();
            let file = parent_dir.join("fetch.txt");
            std::fs::write(&file, "full page").unwrap();
            parent.enter_turn(TurnId::new());
            parent
                .append_message(&Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![orca_core::conversation::RawToolCall {
                        id: "fetch".into(),
                        function_name: "web_fetch".into(),
                        arguments: "{}".into(),
                    }],
                    cut_short: None,
                    pinned: false,
                })
                .unwrap();
            let request = orca_core::tool_types::ToolRequest {
                id: "fetch".into(),
                name: orca_core::tool_types::ToolName::WebFetch,
                action: orca_core::approval_types::ActionKind::Network,
                target: None,
                raw_arguments: None,
            };
            let mut result = ToolResult::completed(&request, "head\n\ntail".into(), true);
            result.set_spill(
                orca_core::tool_types::ToolOutputSpill::new(parent_dir, request.id.clone(), 42)
                    .unwrap(),
            );
            super::record_tool_result_for_agent(
                &mut Conversation::new(),
                Some(&mut parent),
                &result,
                true,
            )
            .unwrap();
            // A result shown on the event stream may also be persisted in JSONL.
            use orca_core::event_schema::{
                EVENT_SCHEMA_VERSION, EventEnvelope, EventFactory, EventPublicationStore,
            };
            result.output = Some(result.spill.as_ref().unwrap().render("head\n\ntail"));
            let event = EventFactory::new("fork-spill".into()).tool_call_completed(&result);
            parent
                .append_semantic_event(&EventEnvelope {
                    version: EVENT_SCHEMA_VERSION.into(),
                    run_id: event.run_id,
                    seq: 0,
                    timestamp_ms: 0,
                    event_type: event.event_type,
                    payload: event.payload,
                })
                .unwrap();
            parent
                .append_message(&Message::Assistant {
                    content: Some("kept reply".into()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    cut_short: Some(CutShort::Interrupted),
                    pinned: false,
                })
                .unwrap();
            drop(parent);
            let store = crate::thread_store::JsonlThreadStore::new();
            let exported = home.join("portable-spill.jsonl");
            store.export_session(&parent_id, &exported).unwrap();
            let portable = std::fs::read_to_string(&exported).unwrap();
            assert!(!portable.contains(serde_json::to_string(&file).unwrap().trim_matches('"')));
            assert!(portable.contains("The full result was not exported."));
            let fork = InteractiveSession::new_with_preloaded(
                &config(cwd.path().to_owned(), HistoryMode::Fork(parent_id.clone())),
                "fork",
                None,
            )
            .unwrap();
            let fork_dir = fork.task_registry().spill_directory().unwrap();
            let fork_file = fork_dir.join("fetch.txt");
            assert!(fork.conversation().messages.iter().any(|message| {
                message
                    .content_str()
                    .is_some_and(|text| text.contains(&fork_file.display().to_string()))
            }));
            assert!(fork.conversation().messages.iter().any(|message| matches!(
                message,
                Message::Assistant {
                    content: Some(content),
                    cut_short: Some(CutShort::Interrupted),
                    ..
                } if content == "kept reply"
            )));
            history::delete_session(&parent_id).unwrap();
            assert_eq!(std::fs::read_to_string(fork_file).unwrap(), "full page");
        });
    }

    #[test]
    fn fork_refuses_a_later_schema_two_meta() {
        use std::io::Write;

        with_orca_home(|home| {
            let parent = history::SessionWriter::start(home, "mock", None, "parent").unwrap();
            let mut newer: serde_json::Value =
                serde_json::from_slice(&std::fs::read(parent.path()).unwrap()).unwrap();
            let parent_id = newer["session_id"].as_str().unwrap().to_owned();
            newer["schema_version"] = serde_json::json!(2);
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(parent.path())
                    .unwrap(),
                "{newer}"
            )
            .unwrap();
            let original = std::fs::read(parent.path()).unwrap();
            let cfg = config(home.to_path_buf(), HistoryMode::Fork(parent_id));

            let error = InteractiveSession::new_with_preloaded(&cfg, "child", None)
                .err()
                .expect("fork must reject the newer parent before creating a child");
            assert!(error.to_string().contains("upgrade Orca"), "{error}");
            let sessions = history::list_sessions_with_archived(usize::MAX, true).unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].path, parent.path());
            assert_eq!(std::fs::read(parent.path()).unwrap(), original);
        });
    }

    #[test]
    fn a_fork_does_not_copy_skipped_records_and_the_parent_keeps_them() {
        use std::io::Write;

        with_orca_home(|home| {
            let mut parent = history::SessionWriter::start(home, "mock", None, "parent").unwrap();
            parent.enter_turn(TurnId::new());
            parent
                .append_message(&Message::user("before skipped record".into()))
                .unwrap();
            let skipped =
                b"{\"type\":\"future.note\",\"ignorable\":true,\"note\":\"parent only\"}\n";
            std::fs::OpenOptions::new()
                .append(true)
                .open(parent.path())
                .unwrap()
                .write_all(skipped)
                .unwrap();
            parent
                .append_message(&Message::user("after skipped record".into()))
                .unwrap();
            let original = std::fs::read(parent.path()).unwrap();
            let parent_id = history::load_session("latest").unwrap().meta.session_id;
            let cfg = config(home.to_path_buf(), HistoryMode::Fork(parent_id.clone()));

            let mut child = InteractiveSession::new_with_preloaded(&cfg, "child", None).unwrap();
            let child_id = child.session_id().unwrap().to_owned();
            let child_path = child.writer_mut().unwrap().path();
            assert_ne!(child_path, parent.path());
            let child_jsonl = std::fs::read_to_string(child_path).unwrap();
            assert!(!child_jsonl.contains("future.note"));
            assert_eq!(std::fs::read(parent.path()).unwrap(), original);
            let transcript = history::load_session(&child_id).unwrap();
            assert_eq!(transcript.path, child_path);
            assert_eq!(
                transcript.meta.parent_id.as_deref(),
                Some(parent_id.as_str())
            );
            let contents = transcript
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(contents, ["before skipped record", "after skipped record"]);
        });
    }

    #[test]
    fn fork_writer_does_not_copy_parent_prompt_cache_checkpoint() {
        with_orca_home(|home| {
            let mut parent = history::SessionWriter::start(
                home,
                "deepseek",
                Some("deepseek-flash".to_string()),
                "parent",
            )
            .expect("parent writer");
            let turn_id = TurnId::new();
            parent.enter_turn(turn_id.clone());
            parent
                .append_message(&Message::user("parent message".to_string()))
                .expect("parent message");
            parent
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
                .expect("parent checkpoint");
            let parent_id = history::load_session("latest")
                .expect("load parent")
                .meta
                .session_id;
            let parent_transcript = history::load_session(&parent_id).expect("reload parent");
            assert!(
                std::fs::read_to_string(parent.path())
                    .expect("read parent JSONL")
                    .contains("\"type\":\"provider.prompt_cache_checkpoint\"")
            );

            let mut inherited_conversation = Conversation::new();
            inherited_conversation.messages = parent_transcript.messages;
            let store = SessionStore::new();
            let meta = store.create_fork_meta(
                home,
                "deepseek",
                Some("deepseek-flash".to_string()),
                "child",
                parent_id,
            );
            let child = start_writer_with_messages(&store, meta, &inherited_conversation)
                .expect("child writer");

            let child_jsonl = std::fs::read_to_string(child.path()).expect("read child JSONL");
            assert!(!child_jsonl.contains("\"type\":\"provider.prompt_cache_checkpoint\""));
        });
    }

    #[test]
    fn bootstrap_agent_conversation_for_loop_builds_prompt_and_user_turn() {
        let cwd = tempdir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock {
            user: Some("prefers concise output".to_string()),
            project: Some("run cargo test".to_string()),
        };

        let conversation = bootstrap_agent_conversation_for_loop(
            cwd.path(),
            "inspect repo",
            1,
            &SubagentType::General,
            &instructions,
            &RunConfig::default(),
            &memory,
            None,
        );

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], orca_core::conversation::Message::System { content, .. }
                if content.contains("Subagent Role") && content.contains("prefers concise output"))
        );
        assert!(
            matches!(&conversation.messages[1], orca_core::conversation::Message::User { content, .. }
                if content == "inspect repo")
        );
    }

    mod repeat {
        use super::*;
        use orca_core::approval_types::ActionKind;
        use orca_core::conversation::RawToolCall;

        const THIRD: &str = "You have called read_file with the same arguments 3 times in a row. The result will not change; use what you already have, change the arguments, or try a different approach.";
        const PATH_A: &str = r#"{"path":"a"}"#;
        const PATH_B: &str = r#"{"path":"b"}"#;

        fn record_call(
            conversation: &mut Conversation,
            mut writer: Option<&mut SessionWriter>,
            name: ToolName,
            args: &str,
        ) {
            let id = format!("call_{}", conversation.messages.len());
            conversation.add_assistant(
                None,
                None,
                vec![RawToolCall {
                    id: id.clone(),
                    function_name: name.as_str().to_string(),
                    arguments: args.to_string(),
                }],
            );
            if let Some(writer) = writer.as_deref_mut() {
                writer
                    .append_message(conversation.messages.last().unwrap())
                    .unwrap();
            }
            let request = ToolRequest {
                id,
                name,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(args.to_string()),
            };
            let result = ToolResult::completed(&request, "tool output".to_string(), false);
            record_tool_result_for_agent(conversation, writer, &result, true).unwrap();
        }

        fn read(conversation: &mut Conversation, args: &str) {
            record_call(conversation, None, ToolName::ReadFile, args);
        }

        fn reminders(conversation: &Conversation) -> Vec<&str> {
            conversation
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::System {
                        content,
                        pinned: false,
                        ..
                    } if content.starts_with("You have called ") => Some(content.as_str()),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn three_identical_calls_add_the_reminder() {
            let mut conversation = Conversation::new();
            for _ in 0..3 {
                assert!(reminders(&conversation).is_empty());
                read(&mut conversation, PATH_A);
            }
            assert_eq!(reminders(&conversation), [THIRD]);
            assert!(matches!(conversation.messages.as_slice(),
                [.., Message::Tool { content: output, .. }, Message::System { content, pinned: false, .. }]
                    if output == "tool output" && content == THIRD));
        }

        #[test]
        fn the_fifth_reminder_includes_the_arguments() {
            let mut conversation = Conversation::new();
            let args = format!(r#"{{"path":"{}"}}"#, "界".repeat(310));
            for _ in 0..9 {
                read(&mut conversation, &args);
            }
            let excerpt = format!(r#"{{"path":"{}"#, "界".repeat(291));
            let fifth = format!(
                "{} Arguments: {excerpt}",
                THIRD.replace("3 times", "5 times")
            );
            let eighth = format!(
                "{} Arguments: {excerpt}",
                THIRD.replace("3 times", "8 times")
            );
            assert_eq!(
                reminders(&conversation),
                [THIRD, fifth.as_str(), eighth.as_str()]
            );
        }

        #[test]
        fn reminders_wait_until_every_tool_in_the_assistant_group_has_settled() {
            let mut conversation = Conversation::new();
            let calls = (0..3)
                .map(|index| RawToolCall {
                    id: format!("call-{index}"),
                    function_name: "read_file".to_string(),
                    arguments: PATH_A.to_string(),
                })
                .collect();
            conversation.add_assistant(None, None, calls);
            for index in 0..2 {
                let request = ToolRequest {
                    id: format!("call-{index}"),
                    name: ToolName::ReadFile,
                    action: ActionKind::Read,
                    target: None,
                    raw_arguments: Some(PATH_A.to_string()),
                };
                record_tool_result_for_agent(
                    &mut conversation,
                    None,
                    &ToolResult::completed(&request, "tool output".to_string(), false),
                    false,
                )
                .unwrap();
                assert!(reminders(&conversation).is_empty());
            }
            let request = ToolRequest {
                id: "call-2".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(PATH_A.to_string()),
            };
            record_tool_result_for_agent(
                &mut conversation,
                None,
                &ToolResult::completed(&request, "tool output".to_string(), false),
                false,
            )
            .unwrap();

            assert_eq!(reminders(&conversation), [THIRD]);
            assert!(
                matches!(conversation.messages.last(), Some(Message::System { content, .. }) if content == THIRD)
            );
        }

        #[test]
        fn reminder_streak_survives_compaction_of_older_tool_groups() {
            let mut conversation = Conversation::new();
            for _ in 0..3 {
                read(&mut conversation, PATH_A);
            }
            conversation.messages = conversation
                .messages
                .split_off(conversation.messages.len() - 3);
            for _ in 0..2 {
                read(&mut conversation, PATH_A);
            }

            assert_eq!(reminders(&conversation).len(), 2);
            assert!(reminders(&conversation)[1].contains("5 times"));
        }

        #[test]
        fn a_different_call_resets_the_count() {
            let mut conversation = Conversation::new();
            for args in [PATH_A, PATH_A, PATH_B, PATH_A, PATH_A] {
                read(&mut conversation, args);
            }
            assert!(reminders(&conversation).is_empty());
            read(&mut conversation, PATH_A);
            assert_eq!(reminders(&conversation), [THIRD]);
        }

        #[test]
        fn a_user_message_resets_the_count() {
            let mut conversation = Conversation::new();
            read(&mut conversation, PATH_A);
            read(&mut conversation, PATH_A);
            conversation.add_user("Read it again".to_string());
            read(&mut conversation, PATH_A);
            read(&mut conversation, PATH_A);
            assert!(reminders(&conversation).is_empty());
            read(&mut conversation, PATH_A);
            assert_eq!(reminders(&conversation), [THIRD]);
        }

        #[test]
        fn argument_key_order_does_not_change_the_key() {
            let mut conversation = Conversation::new();
            for args in [
                r#"{"a":[{"x":1,"y":2}],"z":3}"#,
                r#"{"z":3,"a":[{"y":2,"x":1}]}"#,
                r#"{"a":[{"y":2,"x":1}],"z":3}"#,
            ] {
                read(&mut conversation, args);
            }
            assert_eq!(reminders(&conversation), [THIRD]);
        }

        #[test]
        fn task_wait_never_triggers_a_reminder() {
            let mut conversation = Conversation::new();
            read(&mut conversation, PATH_A);
            read(&mut conversation, PATH_A);
            for name in [
                ToolName::TaskWait,
                ToolName::TaskReadOutput,
                ToolName::TaskList,
            ] {
                for _ in 0..8 {
                    record_call(&mut conversation, None, name.clone(), "{}");
                }
            }
            assert!(reminders(&conversation).is_empty());
            read(&mut conversation, PATH_A);
            assert_eq!(reminders(&conversation), [THIRD]);
        }

        #[test]
        fn the_reminder_survives_resume_as_a_system_message() {
            with_orca_home(|_| {
                let cwd = tempdir().unwrap();
                let mut writer = SessionWriter::start(cwd.path(), "mock", None, "prompt").unwrap();
                writer.enter_turn(TurnId::new());
                let mut conversation = Conversation::new();
                for _ in 0..3 {
                    record_call(
                        &mut conversation,
                        Some(&mut writer),
                        ToolName::ReadFile,
                        PATH_A,
                    );
                }
                let transcript = history::load_session("latest").unwrap();
                let resumed =
                    crate::history::resume_conversation(&transcript, "system prompt".to_string());
                assert_eq!(reminders(&resumed), [THIRD]);
                assert!(matches!(resumed.messages.as_slice(),
                    [.., Message::Tool { content: output, .. }, Message::System { content, pinned: false, .. }]
                        if output == "tool output" && content == THIRD));
            });
        }
    }

    #[test]
    fn tool_result_persistence_failure_keeps_the_live_result_and_marks_degraded() {
        with_orca_home(|_| {
            let cwd = tempdir().unwrap();
            let mut writer = SessionWriter::start(cwd.path(), "mock", None, "prompt")
                .expect("start session writer");
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            // Make the transcript path unwritable without touching the
            // in-memory ledger: a directory cannot be opened for append.
            let transcript = writer.path().to_path_buf();
            std::fs::remove_file(&transcript).expect("remove transcript");
            std::fs::create_dir(&transcript).expect("replace transcript with directory");

            let request = ToolRequest {
                id: "call-persist-failure".to_string(),
                name: ToolName::ReadFile,
                action: orca_core::approval_types::ActionKind::Read,
                target: None,
                raw_arguments: None,
            };
            let result = ToolResult::completed(&request, "tool output survives".to_string(), false);
            let mut conversation = Conversation::new();

            let recorded =
                record_tool_result_for_agent(&mut conversation, Some(&mut writer), &result, true)
                    .expect("persistence failure must not fail the tool result");

            assert!(!recorded);
            assert!(matches!(
                conversation.messages.last(),
                Some(Message::Tool { content, .. }) if content == "tool output survives"
            ));
            assert!(writer.is_degraded());
        });
    }

    #[test]
    fn record_assistant_response_rejects_empty_payload() {
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let response = CompletedModelResponse::new(
            orca_core::thread_item_projection::ModelResponseIdentity::new(
                orca_core::thread_identity::TurnId::new(),
            ),
            None,
            Some("private thinking".to_string()),
            vec![],
        );
        let mut events = EventFactory::new("empty-assistant-response".to_string());
        let mut sink = EventSink::new(Vec::new(), orca_core::config::OutputFormat::Jsonl);

        let error = record_assistant_response_for_agent(
            &mut conversation,
            &response,
            false,
            &mut events,
            &mut sink,
        )
        .expect_err("reasoning-only assistant must not enter history");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(conversation.messages.len(), 1);
    }

    #[test]
    fn recorded_assistant_responses_age_an_open_plan_into_a_reminder() {
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        conversation.replace_plan_state("[in_progress] inspect\n[pending] patch".to_string());
        let response = CompletedModelResponse::new(
            orca_core::thread_item_projection::ModelResponseIdentity::new(TurnId::new()),
            Some("working".to_string()),
            None,
            vec![],
        );
        let mut events = EventFactory::new("plan-reminder".to_string());
        let mut sink = EventSink::new(Vec::new(), orca_core::config::OutputFormat::Jsonl);

        for _ in 0..orca_core::conversation::PLAN_REMINDER_AFTER_TURNS {
            assert!(
                !conversation
                    .internal_context
                    .render()
                    .contains("[Plan reminder]")
            );
            record_assistant_response_for_agent(
                &mut conversation,
                &response,
                false,
                &mut events,
                &mut sink,
            )
            .expect("record assistant response");
        }

        assert!(
            conversation
                .internal_context
                .render()
                .contains("[Plan reminder]")
        );
    }

    #[test]
    fn agent_tool_results_keep_only_the_three_newest_images() {
        let png = orca_core::tool_images::tool_image("image/png", "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string()).unwrap();
        let mut conversation = Conversation::new();
        for index in 0..5 {
            let request = ToolRequest {
                id: format!("call_{index}"),
                name: ToolName::ReadFile,
                action: orca_core::approval_types::ActionKind::Read,
                target: None,
                raw_arguments: None,
            };
            let result = ToolResult::completed(&request, format!("screenshot {index}"), false)
                .with_images(vec![png.clone()]);
            record_tool_result_for_agent(&mut conversation, None, &result, false).unwrap();
            assert_eq!(result.images, vec![png.clone()]);
        }
        for (index, message) in conversation.messages.iter().enumerate() {
            let Message::Tool {
                images, content, ..
            } = message
            else {
                panic!("expected a tool message");
            };
            if index < 2 {
                assert!(images.is_empty());
                assert!(content.ends_with(orca_core::tool_images::SUPERSEDED_TOOL_IMAGE_NOTE));
                assert_eq!(
                    content
                        .matches(orca_core::tool_images::SUPERSEDED_TOOL_IMAGE_NOTE)
                        .count(),
                    1
                );
            } else {
                assert_eq!(images, std::slice::from_ref(&png));
                assert!(!content.contains(orca_core::tool_images::SUPERSEDED_TOOL_IMAGE_NOTE));
            }
        }
    }

    #[test]
    fn a_tool_results_images_are_recorded_on_its_tool_message() {
        let request = ToolRequest {
            id: "call-screenshot".to_string(),
            name: ToolName::Mcp("mcp__screen__capture".to_string()),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let image = orca_core::conversation::ImageInput {
            source: orca_core::conversation::ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string(),
            },
            detail: orca_core::conversation::ImageDetail::High,
        };
        let result = ToolResult::completed(
            &request,
            "screenshot taken\n[1 image attached]".to_string(),
            false,
        )
        .with_images(vec![image.clone()]);
        let mut conversation = Conversation::new();

        record_tool_result_for_agent(&mut conversation, None, &result, false)
            .expect("record tool result");

        let Some(Message::Tool {
            tool_call_id,
            images,
            ..
        }) = conversation.messages.last()
        else {
            panic!("the tool result must become the last message");
        };
        assert_eq!(tool_call_id, "call-screenshot");
        assert_eq!(images, &vec![image]);
    }

    #[test]
    fn interactive_session_records_initial_conversation_and_backtracks_user() {
        with_orca_home(|home| {
            let cfg = config(home.to_path_buf(), HistoryMode::Record);
            let mut session = InteractiveSession::new_with_preloaded(&cfg, "first prompt", None)
                .expect("session");

            assert!(session.session_id().is_some());
            assert_eq!(session.conversation().messages.len(), 1);

            session
                .conversation_mut()
                .add_user("first prompt".to_string());
            let last = session
                .conversation()
                .messages
                .last()
                .cloned()
                .expect("user message");
            session.append_message(&last);

            assert_eq!(
                session.backtrack_last_user(),
                Some("first prompt".to_string())
            );
            assert_eq!(session.conversation().messages.len(), 1);
        });
    }

    #[test]
    fn interactive_session_resume_replays_preloaded_transcript() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "resume")
                    .expect("writer");
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            writer
                .append_message(&orca_core::conversation::Message::User {
                    content: "previous".to_string(),
                    images: Vec::new(),
                    pinned: false,
                })
                .expect("message");
            writer.complete("success").expect("complete");
            let transcript = history::load_session("latest").expect("transcript");
            let original_session_id = transcript.meta.session_id.clone();
            let cfg = config(
                home.to_path_buf(),
                HistoryMode::Resume(transcript.meta.session_id.clone()),
            );

            let session =
                InteractiveSession::new_with_preloaded(&cfg, "resumed prompt", Some(transcript))
                    .expect("session");

            assert_eq!(session.session_id(), Some(original_session_id.as_str()));
            assert!(
                session
                    .conversation()
                    .messages
                    .iter()
                    .any(|message| matches!(
                        message,
                        orca_core::conversation::Message::User { content, .. }
                            if content == "previous"
                    ))
            );
        });
    }

    #[test]
    fn interrupted_resume_adds_runtime_worktree_hint() {
        with_orca_home(|home| {
            let mut writer = history::SessionWriter::start(
                home,
                "mock",
                Some("auto".to_string()),
                "interrupted",
            )
            .expect("writer");
            writer.complete("interrupted").expect("complete");
            let transcript = history::load_session("latest").expect("transcript");

            let conversation = bootstrap_agent_conversation(
                Some(&transcript),
                Message::system("sys".into()),
                home,
                "continue",
            );

            assert!(conversation.internal_context.render().contains(
                "Previous turn was interrupted; existing workspace edits remain. Inspect git diff before continuing."
            ));
        });
    }

    #[test]
    fn interrupted_resume_exit_does_not_add_runtime_worktree_hint() {
        with_orca_home(|home| {
            let mut writer = history::SessionWriter::start(
                home,
                "mock",
                Some("auto".to_string()),
                "interrupted",
            )
            .expect("writer");
            writer.complete("interrupted").expect("complete");
            let transcript = history::load_session("latest").expect("transcript");

            let conversation = bootstrap_agent_conversation(
                Some(&transcript),
                Message::system("sys".into()),
                home,
                "/exit",
            );

            assert!(
                !conversation
                    .internal_context
                    .render()
                    .contains("Previous turn was interrupted; existing workspace edits remain.")
            );
        });
    }

    #[test]
    fn interactive_session_resume_restores_compressed_transcript_before_appending() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "resume")
                    .expect("writer");
            writer.enter_turn(orca_core::thread_identity::TurnId::new());
            writer
                .append_message(&orca_core::conversation::Message::User {
                    content: "previous".to_string(),
                    images: Vec::new(),
                    pinned: false,
                })
                .expect("message");
            writer.complete("success").expect("complete");
            let session_id = history::load_session("latest")
                .expect("transcript")
                .meta
                .session_id;
            let compressed_path = history::compress_session(&session_id).expect("compress");
            assert_eq!(
                compressed_path.extension().and_then(|e| e.to_str()),
                Some("zst")
            );
            let transcript = history::load_session(&session_id).expect("compressed transcript");
            let cfg = config(home.to_path_buf(), HistoryMode::Resume(session_id.clone()));

            let mut session =
                InteractiveSession::new_with_preloaded(&cfg, "resumed prompt", Some(transcript))
                    .expect("session");
            session
                .conversation_mut()
                .add_user("after resume".to_string());
            let last = session
                .conversation()
                .messages
                .last()
                .cloned()
                .expect("user message");
            session.append_message(&last);

            // Appending must not leave plaintext bytes behind a zstd frame:
            // the whole history has to stay readable.
            assert!(!compressed_path.exists());
            let reloaded = history::load_session(&session_id).expect("history stays readable");
            let contents: Vec<_> = reloaded
                .messages
                .iter()
                .filter_map(|message| match message {
                    orca_core::conversation::Message::User { content, .. } => {
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert!(contents.contains(&"previous"));
            assert!(contents.contains(&"after resume"));
        });
    }

    #[test]
    fn interactive_session_reports_active_workflows_from_runtime_registry() {
        with_orca_home(|home| {
            let cfg = config(home.to_path_buf(), HistoryMode::Record);
            let session =
                InteractiveSession::new_with_preloaded(&cfg, "workflow", None).expect("session");

            assert!(!session.has_active_workflows());
            assert!(session.store().list_sessions(10).is_ok());
            let handle = session.task_registry().create_workflow(
                "run-1".to_string(),
                "demo".to_string(),
                "demo workflow".to_string(),
                1,
            );
            session
                .task_registry()
                .mark_running(&handle.id)
                .expect("running");

            assert!(session.has_active_workflows());
            assert_eq!(
                session
                    .task_registry()
                    .get(&handle.id)
                    .expect("task")
                    .status,
                TaskStatus::Running
            );
        });
    }

    #[test]
    fn complete_with_terminal_derives_transcript_status_from_typed_terminal() {
        use orca_core::budget::{BudgetUsage, OperationTerminal, StopReason};

        with_orca_home(|home| {
            let cfg = config(home.to_path_buf(), HistoryMode::Record);
            let mut session = InteractiveSession::new_with_preloaded(&cfg, "typed terminal", None)
                .expect("session");

            session.complete_with_terminal(&OperationTerminal::Completed {
                usage: BudgetUsage::default(),
            });
            assert_eq!(session.completion_error, None);

            let failed = OperationTerminal::Failed {
                class: orca_core::budget::FailureClass::Runtime,
                message: "boom".to_string(),
            };
            session.complete_with_terminal(&failed);
            assert_eq!(
                session.completion_error.as_deref(),
                Some("boom"),
                "Failed terminal message flows to the transcript error"
            );

            let stopped = OperationTerminal::Stopped {
                reason: StopReason::TurnBudget { max_turns: 3 },
                usage: BudgetUsage::default(),
                checkpoint_id: "cp-1".to_string(),
                resumable: true,
            };
            session.complete_with_terminal(&stopped);
            assert_eq!(session.completion_error, None);

            // The saved transcript carries the derived status, not a
            // projection-invented one.
            let store = session.store();
            let sessions = store.list_sessions(10).expect("list sessions");
            assert!(!sessions.is_empty());
        });
    }
}
