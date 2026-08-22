use std::io;
use std::path::Path;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::OutputFormat;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::hook_types::HookEvent;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::subagent_types::SubagentType;
use orca_provider::{ProviderConfig, context};

use crate::hooks::{HookContext, HookRunner, conversation_with_hook_context};
use crate::lifecycle::RuntimeTurnContext;
use crate::session::InteractiveSession;
use crate::thread_store::SessionWriter;
use orca_core::conversation::Conversation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionTrigger {
    SoftLimit,
    HardLimit,
    PromptTooLong,
}

impl RuntimeCompactionTrigger {
    pub(crate) fn reason(self) -> RuntimeCompactionReason {
        match self {
            Self::SoftLimit => RuntimeCompactionReason::ApproachingContextLimit,
            Self::HardLimit => RuntimeCompactionReason::ExceededContextLimit,
            Self::PromptTooLong => RuntimeCompactionReason::PromptTooLongRecovery,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionStrategy {
    LocalTruncation,
    RemoteSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionReason {
    ApproachingContextLimit,
    ExceededContextLimit,
    PromptTooLongRecovery,
}

impl RuntimeCompactionReason {
    pub(crate) fn status_text(&self) -> &'static str {
        match self {
            Self::ApproachingContextLimit => "compacted context near token limit",
            Self::ExceededContextLimit => "compacted context at token limit",
            Self::PromptTooLongRecovery => "compacted context after prompt-too-long",
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::ApproachingContextLimit => "approaching_context_limit",
            Self::ExceededContextLimit => "exceeded_context_limit",
            Self::PromptTooLongRecovery => "prompt_too_long_recovery",
        }
    }
}

impl RuntimeCompactionStrategy {
    pub(crate) fn from_compaction_kind(kind: &context::CompactionKind) -> Self {
        match kind {
            context::CompactionKind::LocalTruncation => Self::LocalTruncation,
            context::CompactionKind::RemoteSummary(_) => Self::RemoteSummary,
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::LocalTruncation => "local_truncation",
            Self::RemoteSummary => "remote_summary",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionDetails {
    pub(crate) trigger: RuntimeCompactionTrigger,
    pub(crate) reason: RuntimeCompactionReason,
    pub(crate) strategy: RuntimeCompactionStrategy,
    pub(crate) before_messages: usize,
    pub(crate) after_messages: usize,
    pub(crate) collapsed_messages: usize,
    pub(crate) status_text: &'static str,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionRetryState {
    prompt_too_long_retried: bool,
}

impl RuntimeCompactionRetryState {
    pub(crate) fn record_prompt_too_long_retry(&mut self) {
        self.prompt_too_long_retried = true;
    }

    #[cfg(test)]
    pub(crate) fn has_prompt_too_long_retry(&self) -> bool {
        self.prompt_too_long_retried
    }

    pub(crate) fn reset(&mut self) {
        self.prompt_too_long_retried = false;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TuiAgentTurnCompactionState {
    retry: RuntimeCompactionRetryState,
}

impl TuiAgentTurnCompactionState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub struct TuiAgentTurnCompactionInput<'a, W: io::Write> {
    pub provider: orca_core::config::ProviderKind,
    pub context_config: &'a context::ContextConfig,
    pub provider_config: &'a ProviderConfig,
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub subagent_depth: u32,
    pub subagent_type: &'a SubagentType,
    pub emit_deltas: bool,
    pub cancel: &'a CancelToken,
    pub events: &'a mut EventFactory,
    pub event_observer: Option<Arc<dyn EventObserver>>,
    pub writer: &'a mut W,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiAgentTurnCompactionOutcome {
    pub used_tokens: usize,
    pub limit_tokens: usize,
    pub compacted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiAgentProviderErrorAction {
    NoError,
    RetryAfterCompaction,
    SurfaceError(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionRetryDecision {
    CompactAndRetry {
        trigger: RuntimeCompactionTrigger,
        reason: RuntimeCompactionReason,
    },
    SurfaceError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionOutcome {
    trigger: RuntimeCompactionTrigger,
    before_messages: usize,
    after_messages: usize,
    strategy: RuntimeCompactionStrategy,
}

impl RuntimeCompactionOutcome {
    pub(crate) fn trigger(&self) -> RuntimeCompactionTrigger {
        self.trigger
    }

    pub(crate) fn before_messages(&self) -> usize {
        self.before_messages
    }

    pub(crate) fn after_messages(&self) -> usize {
        self.after_messages
    }

    pub(crate) fn strategy(&self) -> RuntimeCompactionStrategy {
        self.strategy
    }

    pub(crate) fn reason(&self) -> RuntimeCompactionReason {
        self.trigger().reason()
    }

    pub(crate) fn details(&self) -> RuntimeCompactionDetails {
        let reason = self.reason();
        RuntimeCompactionDetails {
            trigger: self.trigger(),
            reason,
            strategy: self.strategy(),
            before_messages: self.before_messages(),
            after_messages: self.after_messages(),
            collapsed_messages: self.before_messages().saturating_sub(self.after_messages()),
            status_text: reason.status_text(),
        }
    }

    pub(crate) fn should_persist_summary_state(&self, emit_deltas: bool) -> bool {
        if !emit_deltas {
            return false;
        }

        match (self.trigger(), self.strategy()) {
            (
                RuntimeCompactionTrigger::SoftLimit
                | RuntimeCompactionTrigger::HardLimit
                | RuntimeCompactionTrigger::PromptTooLong,
                RuntimeCompactionStrategy::LocalTruncation
                | RuntimeCompactionStrategy::RemoteSummary,
            ) => true,
        }
    }
}

pub(crate) struct RuntimeCompactionPolicy<'a> {
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
}

impl<'a> RuntimeCompactionPolicy<'a> {
    pub(crate) fn new(
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
    ) -> Self {
        Self {
            context_config,
            provider_config,
        }
    }

    pub(crate) fn decide(&self, conversation: &Conversation) -> Option<RuntimeCompactionTrigger> {
        let pressure =
            context::context_pressure(conversation, self.context_config, self.provider_config);
        Self::decide_for_pressure(pressure)
    }

    pub(crate) fn decide_for_pressure(
        pressure: context::ContextPressure,
    ) -> Option<RuntimeCompactionTrigger> {
        // Single trigger line: the soft line is the one that fires compaction.
        // Since the soft line is always <= the hard ceiling, it fires first in
        // normal use. The hard ceiling only refines the event label for the
        // pathological case where a single compaction could not get us back
        // under the soft line and tokens kept climbing past the ceiling.
        if !pressure.should_soft_compact {
            return None;
        }
        if pressure.should_hard_compact {
            Some(RuntimeCompactionTrigger::HardLimit)
        } else {
            Some(RuntimeCompactionTrigger::SoftLimit)
        }
    }

    pub(crate) fn decide_for_provider_error(
        error: &str,
        retry_state: &RuntimeCompactionRetryState,
    ) -> RuntimeCompactionRetryDecision {
        if context::is_prompt_too_long_error(error) && !retry_state.prompt_too_long_retried {
            RuntimeCompactionRetryDecision::CompactAndRetry {
                trigger: RuntimeCompactionTrigger::PromptTooLong,
                reason: RuntimeCompactionReason::PromptTooLongRecovery,
            }
        } else {
            RuntimeCompactionRetryDecision::SurfaceError
        }
    }
}

pub fn run_tui_agent_turn_compaction<W: io::Write>(
    session: &mut InteractiveSession,
    input: TuiAgentTurnCompactionInput<'_, W>,
) -> io::Result<TuiAgentTurnCompactionOutcome> {
    let runtime_parts = session.runtime_parts();
    let mut sink = EventSink::new(input.writer, OutputFormat::Jsonl)
        .with_optional_observer(input.event_observer);
    let turn_context = RuntimeTurnContext::new(
        input.cwd,
        input.prompt,
        input.subagent_depth,
        input.emit_deltas,
        input.subagent_type,
    );
    let mut compaction = RuntimeCompactionStep::new(
        input.provider,
        input.context_config,
        input.provider_config,
        turn_context,
        runtime_parts.hooks,
        input.events,
        &mut sink,
        runtime_parts.writer,
    )
    .with_cancel(input.cancel);
    let compacted = compaction.compact_if_needed(runtime_parts.conversation)?;
    let pressure = context::context_pressure(
        runtime_parts.conversation,
        input.context_config,
        input.provider_config,
    );
    Ok(TuiAgentTurnCompactionOutcome {
        used_tokens: pressure.wire_tokens,
        limit_tokens: pressure.soft_limit,
        compacted,
    })
}

pub fn handle_tui_agent_provider_error<W: io::Write>(
    session: &mut InteractiveSession,
    state: &mut TuiAgentTurnCompactionState,
    response: &ProviderResponse,
    input: TuiAgentTurnCompactionInput<'_, W>,
) -> io::Result<TuiAgentProviderErrorAction> {
    let Some(error) = response.steps.iter().find_map(|step| match step {
        ProviderStep::Error(message) => Some(message.clone()),
        _ => None,
    }) else {
        state.retry.reset();
        return Ok(TuiAgentProviderErrorAction::NoError);
    };

    match RuntimeCompactionPolicy::decide_for_provider_error(&error.message, &state.retry) {
        RuntimeCompactionRetryDecision::CompactAndRetry { trigger, .. } => {
            let runtime_parts = session.runtime_parts();
            let mut sink = EventSink::new(input.writer, OutputFormat::Jsonl)
                .with_optional_observer(input.event_observer);
            let turn_context = RuntimeTurnContext::new(
                input.cwd,
                input.prompt,
                input.subagent_depth,
                input.emit_deltas,
                input.subagent_type,
            );
            let mut compaction = RuntimeCompactionStep::new(
                input.provider,
                input.context_config,
                input.provider_config,
                turn_context,
                runtime_parts.hooks,
                input.events,
                &mut sink,
                runtime_parts.writer,
            )
            .with_cancel(input.cancel);
            compaction.compact_after_provider_error_retry(runtime_parts.conversation, trigger)?;
            state.retry.record_prompt_too_long_retry();
            Ok(TuiAgentProviderErrorAction::RetryAfterCompaction)
        }
        RuntimeCompactionRetryDecision::SurfaceError => {
            state.retry.reset();
            Ok(TuiAgentProviderErrorAction::SurfaceError(error.message))
        }
    }
}

pub(crate) struct RuntimeCompactionTask {
    trigger: RuntimeCompactionTrigger,
    before_messages: usize,
}

impl RuntimeCompactionTask {
    pub(crate) fn start(trigger: RuntimeCompactionTrigger, before_messages: usize) -> Self {
        Self {
            trigger,
            before_messages,
        }
    }

    pub(crate) fn finish(
        &self,
        after_messages: usize,
        kind: &context::CompactionKind,
    ) -> RuntimeCompactionOutcome {
        RuntimeCompactionOutcome {
            trigger: self.trigger(),
            before_messages: self.before_messages(),
            after_messages,
            strategy: RuntimeCompactionStrategy::from_compaction_kind(kind),
        }
    }

    pub(crate) fn trigger(&self) -> RuntimeCompactionTrigger {
        self.trigger
    }

    pub(crate) fn before_messages(&self) -> usize {
        self.before_messages
    }
}

pub(crate) struct RuntimeCompactionStep<'a, W: io::Write> {
    provider: orca_core::config::ProviderKind,
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
    turn_context: RuntimeTurnContext<'a>,
    hooks: &'a HookRunner,
    events: &'a mut EventFactory,
    sink: &'a mut EventSink<W>,
    history_writer: Option<&'a mut SessionWriter>,
    cancel: Option<&'a CancelToken>,
}

impl<'a, W: io::Write> RuntimeCompactionStep<'a, W> {
    pub(crate) fn new(
        provider: orca_core::config::ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        turn_context: RuntimeTurnContext<'a>,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            turn_context,
            hooks,
            events,
            sink,
            history_writer,
            cancel: None,
        }
    }

    pub(crate) fn with_cancel(mut self, cancel: &'a CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub(crate) fn compact_if_needed(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<bool> {
        let policy = RuntimeCompactionPolicy::new(self.context_config, self.provider_config);
        let Some(trigger) = policy.decide(conversation) else {
            return Ok(false);
        };
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        self.compact_with_budget_hooks(conversation, trigger)?;
        Ok(true)
    }

    pub(crate) fn compact_after_provider_error_retry(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<()> {
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        let outcome = self.compact_and_persist(conversation, trigger)?;
        self.emit_compaction_completed(&outcome)?;
        Ok(())
    }

    fn emit_compaction_started(
        &mut self,
        trigger: RuntimeCompactionTrigger,
        before_messages: usize,
    ) -> io::Result<()> {
        if self.turn_context.emit_deltas {
            self.sink.emit(
                self.events
                    .context_compaction_started(trigger.reason().as_str(), before_messages),
            )?;
        }
        Ok(())
    }

    pub(crate) fn emit_error(&mut self, error: &str) -> io::Result<()> {
        if self.turn_context.emit_deltas {
            self.sink.emit(self.events.error(error))?;
        }
        Ok(())
    }

    fn compact_with_budget_hooks(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<()> {
        let before_messages = conversation.messages.len();
        let budget_hook_context = HookContext {
            cwd: &self.turn_context.cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: Some(before_messages),
            after_messages: None,
            usage: None,
        };
        let budget_hook = if let Some(cancel) = self.cancel {
            self.hooks
                .run_with_cancel(HookEvent::OnBudgetWarning, budget_hook_context, cancel)
        } else {
            self.hooks
                .run(HookEvent::OnBudgetWarning, budget_hook_context)
        };
        match budget_hook {
            Ok(outcome) if !outcome.injected_context.is_empty() => {
                *conversation = conversation_with_hook_context(conversation, &outcome);
            }
            Err(error) if self.turn_context.emit_deltas => {
                self.sink.emit(
                    self.events
                        .error(&format!("on_budget_warning hook failed: {error}")),
                )?;
            }
            _ => {}
        }

        if self.turn_context.emit_deltas {
            self.run_compaction_hook(HookEvent::PreCompact, before_messages, None)?;
        }

        let outcome = self.compact_and_persist(conversation, trigger)?;

        if self.turn_context.emit_deltas {
            self.run_compaction_hook(
                HookEvent::PostCompact,
                before_messages,
                Some(outcome.after_messages()),
            )?;
        }
        self.emit_compaction_completed(&outcome)?;

        Ok(())
    }

    fn compact_and_persist(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<RuntimeCompactionOutcome> {
        let before_messages = conversation.messages.len();
        let task = RuntimeCompactionTask::start(trigger, before_messages);
        let compaction = if let Some(cancel) = self.cancel {
            context::compact_with_summary_cancellable(
                self.provider,
                conversation,
                self.context_config,
                self.provider_config,
                cancel,
            )
        } else {
            context::compact_with_summary(
                self.provider,
                conversation,
                self.context_config,
                self.provider_config,
            )
        };
        *conversation = compaction.conversation;
        let after_messages = conversation.messages.len();
        let outcome = task.finish(after_messages, &compaction.kind);
        let details = outcome.details();
        if outcome.should_persist_summary_state(self.turn_context.emit_deltas)
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            writer.append_compaction(details.before_messages, details.after_messages)?;
            if details.strategy == RuntimeCompactionStrategy::RemoteSummary
                && let context::CompactionKind::RemoteSummary(summary) = compaction.kind
            {
                writer.append_summary_state(
                    details.before_messages,
                    details.after_messages,
                    summary,
                    &conversation.summary,
                )?;
            }
        }
        Ok(outcome)
    }

    fn emit_compaction_completed(&mut self, outcome: &RuntimeCompactionOutcome) -> io::Result<()> {
        if self.turn_context.emit_deltas {
            let details = outcome.details();
            self.sink.emit(self.events.context_compacted(
                details.reason.as_str(),
                details.strategy.as_str(),
                details.before_messages,
                details.after_messages,
                details.collapsed_messages,
                details.status_text,
            ))?;
        }
        Ok(())
    }

    fn run_compaction_hook(
        &mut self,
        event: HookEvent,
        before_messages: usize,
        after_messages: Option<usize>,
    ) -> io::Result<()> {
        let context = HookContext {
            cwd: &self.turn_context.cwd.display().to_string(),
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: Some(before_messages),
            after_messages,
            usage: None,
        };
        let result = if let Some(cancel) = self.cancel {
            self.hooks.run_with_cancel(event, context, cancel)
        } else {
            self.hooks.run(event, context)
        };
        if let Err(error) = result {
            self.sink.emit(
                self.events
                    .error(&format!("{} hook failed: {error}", event.as_str())),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::hook_types::HookConfig;
    use std::fs;

    struct CompactionLifecycleAuditWriter {
        output: Vec<u8>,
        history_path: std::path::PathBuf,
        completed_marker: std::path::PathBuf,
        completed_before_persistence: bool,
    }

    impl io::Write for CompactionLifecycleAuditWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            if !self.completed_marker.exists()
                && String::from_utf8_lossy(&self.output).contains("\"type\":\"context.compacted\"")
            {
                let history = fs::read_to_string(&self.history_path)?;
                self.completed_before_persistence = !history.contains("\"context.collapsed\"");
                fs::write(&self.completed_marker, "completed")?;
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compaction_policy_maps_pressure_to_trigger() {
        let below = context::ContextPressure {
            wire_tokens: 8,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: false,
            should_hard_compact: false,
        };
        let soft = context::ContextPressure {
            wire_tokens: 12,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: true,
            should_hard_compact: false,
        };
        let hard = context::ContextPressure {
            wire_tokens: 24,
            effective_limit: 20,
            soft_limit: 10,
            should_soft_compact: true,
            should_hard_compact: true,
        };

        assert_eq!(RuntimeCompactionPolicy::decide_for_pressure(below), None);
        assert_eq!(
            RuntimeCompactionPolicy::decide_for_pressure(soft),
            Some(RuntimeCompactionTrigger::SoftLimit)
        );
        assert_eq!(
            RuntimeCompactionPolicy::decide_for_pressure(hard),
            Some(RuntimeCompactionTrigger::HardLimit)
        );
    }

    #[test]
    fn compaction_task_records_trigger_and_message_counts() {
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::PromptTooLong, 11);

        assert_eq!(task.trigger(), RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(task.before_messages(), 11);

        let outcome = task.finish(4, &context::CompactionKind::LocalTruncation);

        assert_eq!(outcome.trigger(), RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(outcome.before_messages(), 11);
        assert_eq!(outcome.after_messages(), 4);
        assert_eq!(
            outcome.strategy(),
            RuntimeCompactionStrategy::LocalTruncation
        );
        assert!(outcome.should_persist_summary_state(true));
        assert!(!outcome.should_persist_summary_state(false));
    }

    #[test]
    fn compaction_outcome_records_remote_summary_strategy() {
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::HardLimit, 9);
        let outcome = task.finish(
            3,
            &context::CompactionKind::RemoteSummary("summary".to_string()),
        );

        assert_eq!(outcome.trigger(), RuntimeCompactionTrigger::HardLimit);
        assert_eq!(outcome.before_messages(), 9);
        assert_eq!(outcome.after_messages(), 3);
        assert_eq!(outcome.strategy(), RuntimeCompactionStrategy::RemoteSummary);
    }

    #[test]
    fn compaction_outcome_exposes_reason_and_details() {
        let task = RuntimeCompactionTask::start(RuntimeCompactionTrigger::PromptTooLong, 12);
        let outcome = task.finish(
            5,
            &context::CompactionKind::RemoteSummary("summary".to_string()),
        );

        assert_eq!(
            outcome.reason(),
            RuntimeCompactionReason::PromptTooLongRecovery
        );

        let details = outcome.details();
        assert_eq!(details.trigger, RuntimeCompactionTrigger::PromptTooLong);
        assert_eq!(
            details.reason,
            RuntimeCompactionReason::PromptTooLongRecovery
        );
        assert_eq!(details.strategy, RuntimeCompactionStrategy::RemoteSummary);
        assert_eq!(details.before_messages, 12);
        assert_eq!(details.after_messages, 5);
        assert_eq!(details.collapsed_messages, 7);
        assert_eq!(
            details.status_text,
            "compacted context after prompt-too-long"
        );
    }

    #[test]
    fn compaction_policy_decides_prompt_too_long_retry_once() {
        let mut retry_state = RuntimeCompactionRetryState::default();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: prompt_too_long: context length exceeded",
            &retry_state,
        );

        assert_eq!(
            decision,
            RuntimeCompactionRetryDecision::CompactAndRetry {
                trigger: RuntimeCompactionTrigger::PromptTooLong,
                reason: RuntimeCompactionReason::PromptTooLongRecovery,
            }
        );

        retry_state.record_prompt_too_long_retry();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: prompt_too_long: context length exceeded",
            &retry_state,
        );

        assert_eq!(decision, RuntimeCompactionRetryDecision::SurfaceError);
    }

    #[test]
    fn compaction_policy_surfaces_non_context_errors() {
        let retry_state = RuntimeCompactionRetryState::default();
        let decision = RuntimeCompactionPolicy::decide_for_provider_error(
            "DeepSeek provider error: quota exhausted",
            &retry_state,
        );

        assert_eq!(decision, RuntimeCompactionRetryDecision::SurfaceError);
    }

    #[test]
    fn compaction_step_emits_started_before_compacted_event() {
        let context_config = context::ContextConfig {
            max_tokens: 100,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(100),
            soft_compact_token_limit: Some(40),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("compaction-event-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, orca_core::config::OutputFormat::Jsonl);
        let cwd = std::path::Path::new(".");
        let subagent_type = orca_core::subagent_types::SubagentType::General;
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());
        for index in 0..20 {
            conversation.add_user(format!("user message {index}: {}", "context ".repeat(8)));
            conversation.add_assistant(
                Some(format!(
                    "assistant message {index}: {}",
                    "details ".repeat(8)
                )),
                None,
                vec![],
            );
        }
        let before_messages = conversation.messages.len();

        RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &context_config,
            &provider_config,
            RuntimeTurnContext::new(cwd, "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .compact_after_provider_error_retry(
            &mut conversation,
            RuntimeCompactionTrigger::PromptTooLong,
        )
        .expect("compaction should emit event");

        let output = String::from_utf8(output).expect("jsonl is utf8");
        let emitted = output
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("event should be json")
            })
            .collect::<Vec<_>>();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0]["type"], "context.compaction.started");
        assert_eq!(emitted[1]["type"], "context.compacted");
        assert_eq!(emitted[0]["payload"]["reason"], "prompt_too_long_recovery");
        assert_eq!(emitted[0]["payload"]["before_messages"], before_messages);
        let event = &emitted[1];

        assert_eq!(event["payload"]["before_messages"], before_messages);
        assert_eq!(
            event["payload"]["after_messages"],
            conversation.messages.len()
        );
        assert_eq!(event["payload"]["reason"], "prompt_too_long_recovery");
        assert_eq!(event["payload"]["strategy"], "remote_summary");
    }

    #[test]
    fn automatic_compaction_completes_after_persistence_and_post_hook() {
        let temp = tempfile::tempdir().expect("tempdir");
        let history_path = temp.path().join("session.jsonl");
        let meta = crate::history::create_meta(temp.path(), "mock", None, "compaction order");
        let mut meta_record = serde_json::to_value(meta)
            .expect("serialize history metadata")
            .as_object()
            .cloned()
            .expect("history metadata object");
        meta_record.insert("type".to_string(), serde_json::json!("session.meta"));
        fs::write(
            &history_path,
            format!("{}\n", serde_json::Value::Object(meta_record)),
        )
        .expect("seed history file");
        let completed_marker = temp.path().join("completed.marker");
        let hook_command = format!("test ! -e '{}'", completed_marker.display());
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PostCompact,
            command: hook_command,
            tool: None,
        }]);
        let context_config = context::ContextConfig {
            max_tokens: 100,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(100),
            soft_compact_token_limit: Some(40),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());
        for index in 0..20 {
            conversation.add_user(format!("user message {index}: {}", "context ".repeat(8)));
            conversation.add_assistant(
                Some(format!(
                    "assistant message {index}: {}",
                    "details ".repeat(8)
                )),
                None,
                vec![],
            );
        }
        let mut writer = CompactionLifecycleAuditWriter {
            output: Vec::new(),
            history_path: history_path.clone(),
            completed_marker,
            completed_before_persistence: false,
        };
        let mut sink = EventSink::new(&mut writer, orca_core::config::OutputFormat::Jsonl);
        let mut events = EventFactory::new("automatic-compaction-order".to_string());
        let mut history_writer =
            SessionWriter::append_to_existing(history_path).expect("history writer");
        let subagent_type = orca_core::subagent_types::SubagentType::General;

        RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &context_config,
            &provider_config,
            RuntimeTurnContext::new(std::path::Path::new("."), "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            Some(&mut history_writer),
        )
        .compact_if_needed(&mut conversation)
        .expect("automatic compaction");

        assert!(!writer.completed_before_persistence);
        let output = String::from_utf8(writer.output).expect("jsonl is utf8");
        let event_types = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event json"))
            .map(|event| event["type"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec!["context.compaction.started", "context.compacted"]
        );
    }
}
