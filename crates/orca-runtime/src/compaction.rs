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
    Overflow,
    PromptTooLong,
}

impl RuntimeCompactionTrigger {
    pub(crate) fn reason(self) -> RuntimeCompactionReason {
        match self {
            Self::SoftLimit => RuntimeCompactionReason::ApproachingContextLimit,
            Self::HardLimit | Self::Overflow => RuntimeCompactionReason::ExceededContextLimit,
            Self::PromptTooLong => RuntimeCompactionReason::PromptTooLongRecovery,
        }
    }

    /// A request with no room for a minimal reply, and one the provider
    /// rejected as too long, must make room: they force a real reduction.
    /// Pressure past the hard line is ordinary compaction; at the default Max
    /// effort that line is the soft line.
    fn is_emergency(self) -> bool {
        matches!(self, Self::Overflow | Self::PromptTooLong)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeCompactionStrategy {
    LocalTruncation,
    RemoteSummary,
    /// The compacted history did not shrink and was discarded: the history
    /// stays as it was.
    None,
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
            Self::None => "none",
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
            status_text: match self.strategy() {
                RuntimeCompactionStrategy::None => {
                    "context compaction could not shrink the conversation"
                }
                RuntimeCompactionStrategy::LocalTruncation
                | RuntimeCompactionStrategy::RemoteSummary => reason.status_text(),
            },
        }
    }

    pub(crate) fn should_persist_summary_state(&self, _emit_deltas: bool) -> bool {
        match (self.trigger(), self.strategy()) {
            (
                RuntimeCompactionTrigger::SoftLimit
                | RuntimeCompactionTrigger::HardLimit
                | RuntimeCompactionTrigger::Overflow
                | RuntimeCompactionTrigger::PromptTooLong,
                RuntimeCompactionStrategy::LocalTruncation
                | RuntimeCompactionStrategy::RemoteSummary,
            ) => true,
            (
                RuntimeCompactionTrigger::SoftLimit
                | RuntimeCompactionTrigger::HardLimit
                | RuntimeCompactionTrigger::Overflow
                | RuntimeCompactionTrigger::PromptTooLong,
                RuntimeCompactionStrategy::None,
            ) => false,
        }
    }
}

/// Whether a compaction replaced history, and what the next request measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeCompactionAdoption {
    pub(crate) adopted: bool,
    pub(crate) prompt_tokens: usize,
}

pub(crate) fn context_overflow_message(prompt_tokens: usize) -> String {
    format!(
        "The conversation no longer fits the model's context window (about {prompt_tokens} \
         tokens) and compaction cannot shrink it further. Start a new conversation with /new."
    )
}

pub(crate) fn unrecoverable_overflow_message(provider_message: &str) -> String {
    format!(
        "{provider_message} Compaction cannot shrink the conversation further; start a new \
         conversation with /new."
    )
}

// The compacted history carries no usage anchor. Scale its estimate by the
// ratio the provider's count showed for the history it replaces, never down.
fn calibrated_prompt_tokens(estimated: usize, reference: context::PromptMeasurement) -> usize {
    if reference.tokens <= reference.estimated {
        return estimated;
    }
    let scaled = estimated as u128 * reference.tokens as u128 / reference.estimated.max(1) as u128;
    usize::try_from(scaled).unwrap_or(usize::MAX)
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
            let shrunk = compaction
                .compact_after_provider_error_retry(runtime_parts.conversation, trigger)?;
            if shrunk {
                state.retry.record_prompt_too_long_retry();
                Ok(TuiAgentProviderErrorAction::RetryAfterCompaction)
            } else {
                state.retry.reset();
                Ok(TuiAgentProviderErrorAction::SurfaceError(
                    unrecoverable_overflow_message(&error.message),
                ))
            }
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

    /// A compaction whose result did not shrink the history: nothing was
    /// compacted, and the outcome says so.
    pub(crate) fn unchanged(&self) -> RuntimeCompactionOutcome {
        RuntimeCompactionOutcome {
            trigger: self.trigger(),
            before_messages: self.before_messages(),
            after_messages: self.before_messages(),
            strategy: RuntimeCompactionStrategy::None,
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
        Ok(self
            .compact_with_budget_hooks(conversation, trigger)?
            .adopted)
    }

    /// Before a request: compact when pressure calls for it, and as an
    /// emergency when the prompt leaves no room for a minimal reply. `Err`
    /// carries the user-facing message when not even that makes room.
    pub(crate) fn prepare_request(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<Result<(), String>> {
        let measured = context::measure_prompt(conversation, self.provider_config).tokens;
        let trigger = if self.context_config.request_reply_budget(measured).is_none() {
            Some(RuntimeCompactionTrigger::Overflow)
        } else {
            RuntimeCompactionPolicy::decide_for_pressure(context::context_pressure_for_tokens(
                measured,
                self.context_config,
            ))
        };
        let Some(trigger) = trigger else {
            return Ok(Ok(()));
        };
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        let adoption = self.compact_with_budget_hooks(conversation, trigger)?;
        Ok(
            if self
                .context_config
                .request_reply_budget(adoption.prompt_tokens)
                .is_some()
            {
                Ok(())
            } else {
                Err(context_overflow_message(adoption.prompt_tokens))
            },
        )
    }

    pub(crate) fn compact_after_provider_error_retry(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<bool> {
        self.emit_compaction_started(trigger, conversation.messages.len())?;
        let (outcome, adoption) = self.compact_and_persist(conversation, trigger)?;
        self.emit_compaction_completed(&outcome)?;
        Ok(adoption.adopted)
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
    ) -> io::Result<RuntimeCompactionAdoption> {
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

        let (outcome, adoption) = self.compact_and_persist(conversation, trigger)?;

        // PostCompact reports a compaction; a result that did not shrink
        // left the history as it was. The completion event still closes the
        // compacting state the started event opened, saying nothing shrank.
        if adoption.adopted && self.turn_context.emit_deltas {
            self.run_compaction_hook(
                HookEvent::PostCompact,
                before_messages,
                Some(outcome.after_messages()),
            )?;
        }
        self.emit_compaction_completed(&outcome)?;

        Ok(adoption)
    }

    fn compact_and_persist(
        &mut self,
        conversation: &mut Conversation,
        trigger: RuntimeCompactionTrigger,
    ) -> io::Result<(RuntimeCompactionOutcome, RuntimeCompactionAdoption)> {
        let before_messages = conversation.messages.len();
        let task = RuntimeCompactionTask::start(trigger, before_messages);
        let before = context::measure_prompt(conversation, self.provider_config);
        // An emergency must make room even when the local estimate says the
        // prompt fits: force the compaction line under three quarters of the
        // measured prompt so at least one real reduction happens.
        let mut emergency_config;
        let context_config = if trigger.is_emergency() {
            emergency_config = self.context_config.clone();
            // Micro-compaction trims by raw estimate, which an anchored count can exceed.
            emergency_config.soft_compact_token_limit = Some(
                self.context_config
                    .soft_limit()
                    .min(before.tokens.min(before.estimated).saturating_mul(3) / 4)
                    .max(1),
            );
            &emergency_config
        } else {
            self.context_config
        };
        let compaction = if let Some(cancel) = self.cancel {
            context::compact_with_summary_cancellable(
                self.provider,
                conversation,
                context_config,
                self.provider_config,
                cancel,
            )
        } else {
            context::compact_with_summary(
                self.provider,
                conversation,
                context_config,
                self.provider_config,
            )
        };
        let after = context::measure_prompt(&compaction.conversation, self.provider_config);
        if after.estimated >= before.estimated {
            // Nothing shrank: keep the history as it was, and persist nothing.
            return Ok((
                task.unchanged(),
                RuntimeCompactionAdoption {
                    adopted: false,
                    prompt_tokens: before.tokens,
                },
            ));
        }
        let after_messages = compaction.conversation.messages.len();
        let outcome = task.finish(after_messages, &compaction.kind);
        let details = outcome.details();
        if outcome.should_persist_summary_state(self.turn_context.emit_deltas)
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            // Count-only replay cannot reconstruct suffix rewrites, images or
            // pinned turns. Reuse the existing atomic context snapshot record.
            let identity = crate::session::ManualCompactionPersistenceIdentity {
                operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                snapshot_id: uuid::Uuid::now_v7().to_string(),
            };
            writer.append_manual_compaction_snapshot(
                &identity,
                details.before_messages,
                details.strategy.as_str(),
                &compaction.conversation,
            )?;
        }
        *conversation = compaction.conversation;
        conversation.clear_usage_anchor();
        Ok((
            outcome,
            RuntimeCompactionAdoption {
                adopted: true,
                prompt_tokens: calibrated_prompt_tokens(after.estimated, before),
            },
        ))
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
    use orca_core::config::ReasoningEffort;
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
                self.completed_before_persistence =
                    !history.contains("\"context.manual_compaction_snapshot\"");
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
        assert!(outcome.should_persist_summary_state(false));
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
            max_output_tokens: None,
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
            max_output_tokens: None,
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

    #[test]
    fn automatic_compaction_recovers_exact_rewrites_without_event_output() {
        use orca_core::conversation::{Message, RawToolCall};
        crate::history::with_redirected_orca_home("automatic-context-recovery", |home| {
            let temp = tempfile::tempdir().unwrap();
            let sessions = home.join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            let path = sessions.join("session-context-recovery.jsonl");
            let meta =
                crate::history::create_meta(temp.path(), "mock", None, "compaction recovery");
            let mut record = serde_json::to_value(meta).unwrap();
            record["type"] = serde_json::json!("session.meta");
            fs::write(&path, format!("{record}\n")).unwrap();
            let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
            let mut conversation = Conversation::new();
            conversation.add_system("immutable instructions".to_string());
            for index in 0..6 {
                conversation.add_user(format!("inspect {index}"));
                conversation.add_assistant(
                    None,
                    None,
                    vec![RawToolCall {
                        id: format!("call-{index}"),
                        function_name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    }],
                );
                conversation.add_tool_result(format!("call-{index}"), "output ".repeat(1_000));
            }
            conversation.add_user("current request".to_string());
            for message in &conversation.messages {
                writer.append_legacy_message(message).unwrap();
            }
            let provider = ProviderConfig {
                api_key: None,
                base_url: None,
                model: None,
                reasoning_effort: Default::default(),
                tools_override: Some(vec![]),
                mcp_registry: None,
                external_tools: vec![],
                max_output_tokens: None,
            };
            let tokens = context::wire_equivalent_tokens(&conversation, &provider);
            let config = context::ContextConfig {
                max_tokens: tokens + 100,
                compaction_threshold: 1.0,
                reserved_for_response: 0,
                auto_compact_token_limit: Some(tokens + 100),
                soft_compact_token_limit: Some(tokens - 50),
            };
            let hooks = HookRunner::default();
            let mut events = EventFactory::new("context-recovery".to_string());
            let mut output = vec![];
            let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
            let subagent = SubagentType::General;
            let before = conversation.messages.len();
            RuntimeCompactionStep::new(
                orca_core::config::ProviderKind::Mock,
                &config,
                &provider,
                RuntimeTurnContext::new(temp.path(), "", 0, false, &subagent),
                &hooks,
                &mut events,
                &mut sink,
                Some(&mut writer),
            )
            .compact_if_needed(&mut conversation)
            .unwrap();
            assert_eq!(conversation.messages.len(), before);
            assert!(conversation.messages.iter().any(|message| matches!(message,
            Message::Tool { content, .. } if content.starts_with("[tool output micro-compact]"))));
            assert!(output.is_empty());
            let transcript = crate::thread_store::JsonlThreadStore::new()
                .load_session("context-recovery")
                .unwrap();
            let recovered = crate::history::resume_conversation(
                &transcript,
                "immutable instructions".to_string(),
            );
            let before_checkpoint = orca_provider::prompt_cache::checkpoint_for_deepseek_request(
                &conversation,
                &provider,
            )
            .unwrap();
            assert!(
                before_checkpoint
                    .matches_deepseek_prefix(&recovered, &provider)
                    .unwrap()
            );
            let again = context::compact_with_summary(
                orca_core::config::ProviderKind::Mock,
                &recovered,
                &config,
                &provider,
            );
            assert!(
                before_checkpoint
                    .matches_deepseek_prefix(&again.conversation, &provider)
                    .unwrap()
            );
        });
    }

    #[test]
    fn delivered_notices_survive_compaction_and_resume_unpinned() {
        use orca_core::conversation::{Message, RawToolCall};
        crate::history::with_redirected_orca_home("delivered-notice-resume", |home| {
            let temp = tempfile::tempdir().unwrap();
            let sessions = home.join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            let path = sessions.join("session-notice-resume.jsonl");
            let meta = crate::history::create_meta(temp.path(), "mock", None, "notice resume");
            let mut record = serde_json::to_value(meta).unwrap();
            record["type"] = serde_json::json!("session.meta");
            fs::write(&path, format!("{record}\n")).unwrap();
            let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
            let mut conversation = Conversation::new();
            conversation.add_system("immutable instructions".to_string());
            for index in 0..6 {
                conversation.add_user(format!("inspect {index}"));
                conversation.add_assistant(
                    None,
                    None,
                    vec![RawToolCall {
                        id: format!("call-{index}"),
                        function_name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    }],
                );
                conversation.add_tool_result(format!("call-{index}"), "output ".repeat(1_000));
            }
            for message in &conversation.messages {
                writer.append_legacy_message(message).unwrap();
            }
            // Delivered the way a turn boundary delivers a finished task.
            let notice = "<task-notification>Terminal session s-1 (task t-1) finished with \
                          status completed and exit code 0.\nbuild ok</task-notification>"
                .to_string();
            assert!(writer.append_system_delivery_once(&notice).unwrap());
            conversation.add_system(notice.clone());
            // Ephemeral runtime context is rebuilt on resume, not restored.
            let ephemeral = Message::system("ephemeral runtime note".to_string());
            writer.append_legacy_message(&ephemeral).unwrap();
            conversation.messages.push(ephemeral);
            let request = Message::user("current request".to_string());
            writer.append_legacy_message(&request).unwrap();
            conversation.messages.push(request);
            let provider = bare_provider_config();
            let tokens = context::wire_equivalent_tokens(&conversation, &provider);
            let config = context::ContextConfig {
                max_tokens: tokens + 100,
                compaction_threshold: 1.0,
                reserved_for_response: 0,
                auto_compact_token_limit: Some(tokens + 100),
                soft_compact_token_limit: Some(tokens - 50),
            };
            let hooks = HookRunner::default();
            let mut events = EventFactory::new("notice-resume".to_string());
            let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
            let subagent = SubagentType::General;

            let adopted = RuntimeCompactionStep::new(
                orca_core::config::ProviderKind::Mock,
                &config,
                &provider,
                RuntimeTurnContext::new(temp.path(), "", 0, false, &subagent),
                &hooks,
                &mut events,
                &mut sink,
                Some(&mut writer),
            )
            .compact_if_needed(&mut conversation)
            .unwrap();
            assert!(adopted, "the compaction must write a snapshot");
            assert!(conversation.messages.iter().any(|message| matches!(message,
                Message::System { content, pinned: false } if content == &notice)));

            let transcript = crate::thread_store::JsonlThreadStore::new()
                .load_session("notice-resume")
                .unwrap();
            let resumed = crate::history::resume_conversation(&transcript, "fresh".to_string());
            let system = resumed
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::System { content, pinned } => Some((content.as_str(), *pinned)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(system, vec![("fresh", false), (notice.as_str(), false)]);
        });
    }

    #[test]
    fn automatic_compaction_snapshot_failure_keeps_live_conversation_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("context.jsonl");
        let meta = crate::history::create_meta(temp.path(), "mock", None, "compaction failure");
        let mut record = serde_json::to_value(meta).unwrap();
        record["type"] = serde_json::json!("session.meta");
        fs::write(&path, format!("{record}\n")).unwrap();
        let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
        SessionWriter::inject_manual_compaction_snapshot_failure_once(path);
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());
        conversation.add_user("old evidence ".repeat(700));
        conversation.add_user("current request".to_string());
        let config = context::ContextConfig {
            max_tokens: 500,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(500),
            soft_compact_token_limit: Some(300),
        };
        let provider = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: Default::default(),
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        };
        let before =
            orca_provider::prompt_cache::checkpoint_for_deepseek_request(&conversation, &provider)
                .unwrap();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("context-failure".to_string());
        let mut output = vec![];
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let subagent = SubagentType::General;
        let result = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider,
            RuntimeTurnContext::new(temp.path(), "", 0, true, &subagent),
            &hooks,
            &mut events,
            &mut sink,
            Some(&mut writer),
        )
        .compact_if_needed(&mut conversation);
        assert!(result.is_err());
        assert!(
            before
                .matches_deepseek_prefix(&conversation, &provider)
                .unwrap()
        );
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("\"type\":\"context.compacted\"")
        );
    }

    #[test]
    fn ordinary_compaction_that_cannot_shrink_is_not_adopted() {
        // Soft line only (SoftLimit), then soft == hard (HardLimit).
        for (soft, hard) in [(300, 100_000), (300, 300)] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("context.jsonl");
            let meta =
                crate::history::create_meta(temp.path(), "mock", None, "unshrinkable compaction");
            let mut record = serde_json::to_value(meta).unwrap();
            record["type"] = serde_json::json!("session.meta");
            fs::write(&path, format!("{record}\n")).unwrap();
            let mut writer = SessionWriter::append_to_existing(path.clone()).unwrap();
            // Only the current request, too large on its own: nothing to collapse or trim.
            let mut conversation = Conversation::new();
            conversation.add_system("immutable instructions".to_string());
            conversation.add_user("evidence ".repeat(2_000));
            let before = format!("{:?}", conversation.messages);
            let config = context::ContextConfig {
                max_tokens: 100_000,
                compaction_threshold: 1.0,
                reserved_for_response: 0,
                auto_compact_token_limit: Some(hard),
                soft_compact_token_limit: Some(soft),
            };
            let hooks = HookRunner::default();
            let mut events = EventFactory::new("unshrinkable-compaction".to_string());
            let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
            let subagent_type = SubagentType::General;

            let adopted = RuntimeCompactionStep::new(
                orca_core::config::ProviderKind::Mock,
                &config,
                &bare_provider_config(),
                RuntimeTurnContext::new(temp.path(), "", 0, false, &subagent_type),
                &hooks,
                &mut events,
                &mut sink,
                Some(&mut writer),
            )
            .compact_if_needed(&mut conversation)
            .expect("compaction");

            assert!(!adopted, "soft {soft}, hard {hard}");
            assert_eq!(format!("{:?}", conversation.messages), before);
            drop(writer);
            assert!(
                !fs::read_to_string(&path)
                    .unwrap()
                    .contains("\"type\":\"context.manual_compaction_snapshot\""),
                "soft {soft}, hard {hard}"
            );
        }
    }

    #[test]
    fn compaction_that_cannot_shrink_skips_post_compact_and_reports_nothing_compacted() {
        let temp = tempfile::tempdir().unwrap();
        let pre_marker = temp.path().join("pre-compact.marker");
        let post_marker = temp.path().join("post-compact.marker");
        let hooks = HookRunner::new(vec![
            HookConfig {
                event: HookEvent::PreCompact,
                command: format!("touch '{}'", pre_marker.display()),
                tool: None,
            },
            HookConfig {
                event: HookEvent::PostCompact,
                command: format!("touch '{}'", post_marker.display()),
                tool: None,
            },
        ]);
        // Only the current request, too large on its own: nothing to collapse or trim.
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        conversation.add_user("evidence ".repeat(2_000));
        let config = context::ContextConfig {
            max_tokens: 100_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(100_000),
            soft_compact_token_limit: Some(300),
        };
        let provider_config = bare_provider_config();
        let mut events = EventFactory::new("unshrinkable-completion".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let mut step = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(temp.path(), "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        );

        assert!(!step.compact_if_needed(&mut conversation).expect("compact"));
        assert!(
            !step
                .compact_after_provider_error_retry(
                    &mut conversation,
                    RuntimeCompactionTrigger::PromptTooLong,
                )
                .expect("compact")
        );

        assert!(pre_marker.exists(), "compaction hooks must run");
        assert!(!post_marker.exists(), "nothing was compacted");
        let completions = String::from_utf8(output)
            .expect("jsonl is utf8")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event json"))
            .filter(|event| event["type"] == "context.compacted")
            .collect::<Vec<_>>();
        // Both entry points still close the compacting state they opened.
        assert_eq!(completions.len(), 2);
        for completion in completions {
            let payload = &completion["payload"];
            assert_eq!(payload["strategy"], "none");
            assert_eq!(
                payload["status_text"],
                "context compaction could not shrink the conversation"
            );
            assert_eq!(payload["before_messages"], 2);
            assert_eq!(payload["after_messages"], 2);
            assert_eq!(payload["collapsed_messages"], 0);
        }
    }

    #[test]
    fn prepare_request_makes_room_for_a_reply_while_pressure_is_quiet() {
        // Both compaction lines sit at the window, so pressure never fires:
        // only the missing room for a minimal reply can start a compaction.
        let config = context::ContextConfig {
            max_tokens: 40_000,
            compaction_threshold: 0.9,
            reserved_for_response: 4_096,
            auto_compact_token_limit: Some(40_000),
            soft_compact_token_limit: Some(40_000),
        };
        let provider_config = bare_provider_config();
        let mut conversation = history_with_turns(8, 4_000);
        let before = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(before < config.soft_limit(), "pressure must stay quiet");
        assert!(
            config.request_reply_budget(before).is_none(),
            "fixture must leave no room for a reply"
        );
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("no-reply-room".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        assert_eq!(prepared, Ok(()));
        let after = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(
            config.request_reply_budget(after).is_some(),
            "compacted {before} down to {after}"
        );
        let started = String::from_utf8(output)
            .expect("jsonl is utf8")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event json"))
            .find(|event| event["type"] == "context.compaction.started")
            .expect("compaction started");
        assert_eq!(started["payload"]["reason"], "exceeded_context_limit");
    }

    fn bare_provider_config() -> ProviderConfig {
        ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::default(),
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        }
    }

    fn step_config(window: usize) -> context::ContextConfig {
        context::ContextConfig {
            max_tokens: window,
            compaction_threshold: 0.9,
            reserved_for_response: 4_096,
            auto_compact_token_limit: None,
            soft_compact_token_limit: None,
        }
    }

    fn history_with_turns(turns: usize, words: usize) -> Conversation {
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        for index in 0..turns {
            conversation.add_user(format!("inspect file {index}"));
            conversation.add_assistant(
                None,
                None,
                vec![orca_core::conversation::RawToolCall {
                    id: format!("call-{index}"),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            );
            conversation.add_tool_result(format!("call-{index}"), "evidence ".repeat(words));
        }
        conversation.add_user("current request".to_string());
        conversation
    }

    #[test]
    fn prepare_request_compacts_when_no_reply_fits() {
        // 40K window, 4,096 reply: a request needs the prompt at or under 27,712.
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("prepare-request".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = history_with_turns(8, 4_000);
        let before = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(
            config.request_reply_budget(before).is_none(),
            "fixture must overflow"
        );

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        assert_eq!(prepared, Ok(()));
        // Micro-compaction may keep every message and shorten old tool
        // output instead, so compare the measured size, not the count.
        let after = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(after < before);
        // The room comes from the Overflow emergency line: ordinary compaction
        // only aims for 9/10 of the 31,904 soft line (28,713), above 27,712.
        assert!(config.request_reply_budget(after).is_some());
    }

    #[test]
    fn only_the_two_spec_entry_points_are_emergencies() {
        // Pressure past the hard line is ordinary compaction: at the default
        // Max effort the soft and hard lines coincide.
        assert!(!RuntimeCompactionTrigger::SoftLimit.is_emergency());
        assert!(!RuntimeCompactionTrigger::HardLimit.is_emergency());
        assert!(RuntimeCompactionTrigger::Overflow.is_emergency());
        assert!(RuntimeCompactionTrigger::PromptTooLong.is_emergency());
        // Overflow reports like the hard line, so emitted events do not change.
        assert_eq!(
            RuntimeCompactionTrigger::Overflow.reason(),
            RuntimeCompactionTrigger::HardLimit.reason()
        );
    }

    #[test]
    fn prepare_request_compacts_normally_while_a_reply_still_fits() {
        // Soft line == hard line == 30,000, like the default Max effort's
        // 768,928. A prompt just past it still leaves room for a reply.
        let config = context::ContextConfig {
            max_tokens: 60_000,
            compaction_threshold: 0.9,
            reserved_for_response: 24_000,
            auto_compact_token_limit: None,
            soft_compact_token_limit: None,
        };
        assert_eq!(config.soft_limit(), config.effective_limit());
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("prepare-request".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = history_with_turns(16, 2_000);
        let before = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(before > config.soft_limit(), "fixture must pass the line");
        assert!(
            config.request_reply_budget(before).is_some(),
            "fixture must leave room for a reply"
        );

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        assert_eq!(prepared, Ok(()));
        let after = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(after <= config.soft_limit() * 9 / 10, "normal micro target");
        // An emergency would have forced the line under 3/4 of the prompt.
        assert!(after > before * 3 / 4, "compacted {before} down to {after}");
    }

    #[test]
    fn prepare_request_stops_when_compaction_cannot_make_room() {
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("prepare-request".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        // Only the current request, too large on its own: nothing to collapse.
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        conversation.add_user("evidence ".repeat(40_000));
        let before = conversation.messages.clone();

        let prepared = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .prepare_request(&mut conversation)
        .expect("prepare");

        let message = prepared.expect_err("no room for a reply");
        assert!(message.contains("/new"), "{message}");
        assert_eq!(conversation.messages.len(), before.len());
    }

    #[test]
    fn overflow_retry_is_refused_when_compaction_cannot_shrink() {
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("overflow-retry".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = Conversation::new();
        conversation.add_system("immutable instructions".to_string());
        conversation.add_user("small request".to_string());

        let shrunk = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .compact_after_provider_error_retry(
            &mut conversation,
            RuntimeCompactionTrigger::PromptTooLong,
        )
        .expect("compact");

        assert!(!shrunk);
    }

    #[test]
    fn emergency_compaction_cuts_deep_even_when_the_provider_counts_more() {
        let config = step_config(40_000);
        let provider_config = bare_provider_config();
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("emergency-anchor".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = tempfile::tempdir().expect("cwd");
        let mut conversation = history_with_turns(8, 4_000);
        let estimated = context::wire_equivalent_tokens(&conversation, &provider_config);
        // The provider counted half again the estimate, a ratio the anchor accepts.
        conversation.record_usage_anchor(
            (estimated * 3 / 2) as u64,
            estimated,
            conversation.messages.len(),
        );
        assert_eq!(
            context::measure_prompt(&conversation, &provider_config).tokens,
            estimated * 3 / 2,
            "fixture must measure from the anchor"
        );

        let shrunk = RuntimeCompactionStep::new(
            orca_core::config::ProviderKind::Mock,
            &config,
            &provider_config,
            RuntimeTurnContext::new(cwd.path(), "", 0, false, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        )
        .compact_after_provider_error_retry(
            &mut conversation,
            RuntimeCompactionTrigger::PromptTooLong,
        )
        .expect("compact");

        assert!(shrunk);
        let after = context::wire_equivalent_tokens(&conversation, &provider_config);
        assert!(
            after <= estimated * 3 / 4,
            "estimate {estimated} only shrank to {after}"
        );
    }
}
