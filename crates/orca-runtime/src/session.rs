use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;
use std::io;
use std::path::Path;

use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::{Conversation, Message, assistant_message_has_payload};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::TaskStatus;
use orca_core::thread_item_projection::CompletedModelResponse;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;

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

pub struct InteractiveSession {
    store: SessionStore,
    conversation: Conversation,
    writer: Option<SessionWriter>,
    session_id: Option<String>,
    next_event_seq: u64,
    completion_error: Option<String>,
    instructions: ProjectInstructions,
    cost_tracker: CostTracker,
    usage_baseline: UsageTotals,
    mcp_registry: McpRegistry,
    hooks: HookRunner,
    memory: MemoryBlock,
    auto_memory_worker: Option<memory::AutomaticMemoryWorker>,
    auto_memory_turn_starts: HashMap<String, usize>,
    task_registry: TaskRegistry,
    last_manual_compaction: Option<ManualCompactionOutcome>,
    unsandboxed_shell: bool,
}

pub(crate) struct InteractiveSessionRuntimeParts<'a> {
    pub conversation: &'a mut Conversation,
    pub writer: Option<&'a mut SessionWriter>,
    pub instructions: &'a ProjectInstructions,
    pub cost_tracker: &'a mut CostTracker,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub memory: &'a MemoryBlock,
    pub task_registry: &'a TaskRegistry,
    pub unsandboxed_shell: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ManualCompactionOutcome {
    pub before: Conversation,
    pub after: Conversation,
    pub strategy: &'static str,
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
) -> io::Result<String> {
    let result_content = agent_common::format_tool_result_for_model(result);
    let message = Message::Tool {
        tool_call_id: result.id.clone(),
        content: result_content.clone(),
        terminal: Some(result.terminal().clone()),
        pinned: false,
    };
    let history_result = if emit_deltas && let Some(writer) = history_writer {
        writer.append_message(&message)
    } else {
        Ok(())
    };
    conversation.messages.push(message);
    history_result?;
    Ok(result_content)
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
    if emit_deltas {
        sink.emit(events.model_response_completed(response))?;
    }
    conversation.messages.push(response.assistant_message());
    Ok(())
}

pub(crate) fn bootstrap_agent_conversation(
    resumed: Option<&SessionTranscript>,
    system_prompt: String,
    cwd: &Path,
    prompt: &str,
) -> Conversation {
    let mut conversation = if let Some(resumed) = resumed {
        let mut conv = crate::thread_store::resume_conversation(resumed, system_prompt);
        conv.strip_legacy_pinned_volatile();
        conv.strip_legacy_summary_messages();
        conv
    } else {
        let mut conversation = Conversation::new();
        conversation.add_system(system_prompt);
        conversation
    };
    if resumed.and_then(|transcript| transcript.completion_status.as_deref()) == Some("interrupted")
        && prompt.trim() != "/exit"
    {
        conversation.replace_runtime_context(Some(INTERRUPTED_RESUME_HINT.to_string()));
    }
    conversation.replace_skill_context(agent_common::explicit_skill_context(cwd, prompt));
    conversation.add_user(prompt.to_string());
    conversation
}

pub(crate) fn bootstrap_agent_conversation_for_loop(
    cwd: &Path,
    prompt: &str,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    approval_mode: orca_core::approval_types::ApprovalMode,
    memory: &MemoryBlock,
) -> Conversation {
    let system_prompt = agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        approval_mode,
        Some(memory),
    );
    bootstrap_agent_conversation(None, system_prompt, cwd, prompt)
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
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<SessionTranscript>,
    ) -> io::Result<Self> {
        let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
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
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let store = SessionStore::new();
        let instructions = instructions::load_for_cwd_or_default(&cwd);
        let memory = memory::load_for_cwd(&cwd);
        let hooks = HookRunner::new(config.hooks.clone());
        let system_prompt = agent_common::build_agent_system_prompt(
            &cwd,
            0,
            &SubagentType::General,
            Some(&instructions),
            config.approval_mode,
            Some(&memory),
        );
        let (conversation, loaded_transcript) = match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => {
                let transcript = match preloaded {
                    Some(t) => t,
                    None => store.load_session(selector)?,
                };
                let mut conv = store.resume_conversation(&transcript, system_prompt);
                conv.strip_legacy_pinned_volatile();
                conv.strip_legacy_summary_messages();
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
                let mut conv = store.resume_conversation(&transcript, system_prompt);
                conv.strip_legacy_pinned_volatile();
                conv.strip_legacy_summary_messages();
                (conv, Some(transcript))
            }
            HistoryMode::Record | HistoryMode::Disabled => {
                let mut conversation = Conversation::new();
                conversation.add_system(system_prompt);
                (conversation, None)
            }
        };
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
        let next_event_seq = if matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::ResumeAt { .. }
        ) {
            loaded_transcript
                .as_ref()
                .map(|transcript| transcript.next_event_seq)
                .unwrap_or_default()
        } else {
            0
        };

        let unsandboxed_shell = loaded_transcript
            .as_ref()
            .is_some_and(|transcript| transcript.meta.unsandboxed_shell)
            || prepared_record_meta
                .as_ref()
                .is_some_and(|meta| meta.unsandboxed_shell);
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
                        Ok(writer) => {
                            session_id = Some(thread_session_id);
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
                session_id = Some(meta.session_id.clone());
                start_writer_with_messages(&store, meta, &conversation)
            }
        };

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
        };

        let auto_memory_worker = (config.auto_memory
            && !matches!(config.history_mode, HistoryMode::Disabled))
        .then(memory::AutomaticMemoryWorker::start)
        .flatten();
        let session = Self {
            store,
            conversation,
            writer,
            session_id,
            next_event_seq,
            completion_error: None,
            instructions,
            cost_tracker: CostTracker::new(None),
            usage_baseline,
            mcp_registry,
            hooks,
            memory,
            auto_memory_worker,
            auto_memory_turn_starts: HashMap::new(),
            task_registry,
            last_manual_compaction: None,
            unsandboxed_shell,
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

    pub(crate) fn conversation_records(&self) -> Option<Vec<StoredConversationRecord>> {
        self.writer
            .as_ref()
            .map(SessionWriter::conversation_records)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn unsandboxed_shell(&self) -> bool {
        self.unsandboxed_shell
    }

    pub(crate) fn event_publication_store(&self) -> Option<(u64, SessionWriter)> {
        self.writer
            .as_ref()
            .cloned()
            .map(|writer| (self.next_event_seq, writer))
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
        let current = self.cost_tracker.totals();
        UsageTotals {
            input_tokens: self
                .usage_baseline
                .input_tokens
                .saturating_add(current.input_tokens),
            output_tokens: self
                .usage_baseline
                .output_tokens
                .saturating_add(current.output_tokens),
            cache_tokens: self
                .usage_baseline
                .cache_tokens
                .saturating_add(current.cache_tokens),
            estimated_cost_usd: self.usage_baseline.estimated_cost_usd + current.estimated_cost_usd,
        }
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
        self.auto_memory_turn_starts.remove(turn_id);
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

    pub(crate) fn begin_automatic_memory_turn(
        &mut self,
        turn_id: &str,
        existing_turn: bool,
    ) -> usize {
        if existing_turn {
            return self
                .auto_memory_turn_starts
                .get(turn_id)
                .copied()
                .unwrap_or(self.conversation.messages.len());
        }
        self.auto_memory_turn_starts.clear();
        let start = self.conversation.messages.len();
        self.auto_memory_turn_starts
            .insert(turn_id.to_string(), start);
        start
    }

    pub(crate) fn finish_automatic_memory_turn(&mut self, turn_id: &str) {
        self.auto_memory_turn_starts.remove(turn_id);
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

    pub fn task_registry(&self) -> &TaskRegistry {
        &self.task_registry
    }

    pub(crate) fn runtime_parts(&mut self) -> InteractiveSessionRuntimeParts<'_> {
        InteractiveSessionRuntimeParts {
            conversation: &mut self.conversation,
            writer: self.writer.as_mut(),
            instructions: &self.instructions,
            cost_tracker: &mut self.cost_tracker,
            mcp_registry: &self.mcp_registry,
            hooks: &self.hooks,
            memory: &self.memory,
            task_registry: &self.task_registry,
            unsandboxed_shell: self.unsandboxed_shell,
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

    pub(crate) fn set_unsandboxed_shell(&mut self, enabled: bool) {
        self.unsandboxed_shell |= enabled;
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
        let provider_config = ProviderConfig {
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            model: Some(orca_core::model::auxiliary_model().to_string()),
            reasoning_effort: config.reasoning_effort,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let compaction = orca_provider::context::compact_with_summary_cancellable(
            config.provider,
            &candidate,
            &orca_provider::context::ContextConfig::for_model_with_runtime(
                config.model.as_option().as_deref(),
                &config.model_runtime,
            ),
            &provider_config,
            cancel,
        );
        let strategy = match &compaction.kind {
            orca_provider::context::CompactionKind::LocalTruncation => "local_truncation",
            orca_provider::context::CompactionKind::RemoteSummary(_) => "remote_summary",
        };
        let after = compaction.conversation;
        let after_messages = after.messages.len();
        let outcome = ManualCompactionOutcome {
            before,
            after,
            strategy,
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
        self.conversation = outcome.after.clone();
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
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
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
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
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
    fn fork_writer_does_not_copy_parent_prompt_cache_checkpoint() {
        with_orca_home(|home| {
            let mut parent = history::SessionWriter::start(
                home,
                "deepseek",
                Some("deepseek-v4-flash".to_string()),
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
                Some("deepseek-v4-flash".to_string()),
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
            ApprovalMode::Suggest,
            &memory,
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
                "sys".to_string(),
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

            let conversation =
                bootstrap_agent_conversation(Some(&transcript), "sys".to_string(), home, "/exit");

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
