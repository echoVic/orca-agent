use std::io;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, EventPublicationStore, RunStatus};
use orca_mcp::McpRegistry;

use crate::controller::{
    ControllerRunOptions, ThreadTurnExecutor, ThreadTurnOutcome, ThreadTurnRequest,
};
use crate::extension::ExtensionData;
use crate::goal_actor::{GoalRuntimeBinding, GoalRuntimeHandle};
use crate::goal_store::GoalUsageEvent;
use crate::goal_verifier::{
    DeepSeekGoalVerifier, DeterministicGoalVerifier, GoalVerificationRequest, GoalVerifier,
};
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind};
use crate::session::{InteractiveSession, new_run_id};
use crate::thread_store::SessionTranscript;

pub struct RuntimeThread {
    thread_id: String,
    session: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
    thread_extensions: Arc<ExtensionData>,
    next_extension_turn: u64,
    goal_runtime: Option<GoalRuntimeHandle>,
    goal_actor_join: Option<std::thread::JoinHandle<()>>,
}

/// Per-outer-turn activity counters used by the goal no-progress watchdog.
/// Activity is retained for observability, while `has_substantive_progress`
/// deliberately excludes model chatter and completed read-only tools.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TurnProgressEvidence {
    pub(crate) tool_count: u32,
    pub(crate) model_response_count: u32,
    pub(crate) substantive_tool_count: u32,
    pub(crate) plan_changed: bool,
}

impl TurnProgressEvidence {
    #[cfg(test)]
    pub(crate) fn has_activity(&self) -> bool {
        self.tool_count > 0 || self.model_response_count > 0
    }

    pub(crate) fn has_substantive_progress(&self) -> bool {
        self.substantive_tool_count > 0 || self.plan_changed
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TurnProgressBaseline {
    message_count: usize,
    plan_snapshot: Option<String>,
}

impl TurnProgressBaseline {
    pub(crate) fn capture(conversation: &orca_core::conversation::Conversation) -> Self {
        Self {
            message_count: conversation.messages.len(),
            plan_snapshot: plan_snapshot(conversation).map(str::to_string),
        }
    }
}

impl RuntimeThread {
    pub fn start(config: &RunConfig, title: impl Into<String>) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded(config, &title.into(), None)?;
        Ok(Self::from_session(session, None))
    }

    pub fn start_with_preloaded(
        config: &RunConfig,
        title: impl Into<String>,
        preloaded: Option<SessionTranscript>,
    ) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded(config, &title.into(), preloaded)?;
        Ok(Self::from_session(session, None))
    }

    pub fn start_with_preloaded_and_mcp_registry(
        config: &RunConfig,
        title: impl Into<String>,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
    ) -> io::Result<Self> {
        let session = InteractiveSession::new_with_preloaded_and_mcp_registry(
            config,
            &title.into(),
            preloaded,
            mcp_registry,
        )?;
        Ok(Self::from_session(session, None))
    }

    pub(crate) fn start_with_prepared_history_and_runtime_id(
        config: &RunConfig,
        title: impl Into<String>,
        preloaded: Option<SessionTranscript>,
        mcp_registry: McpRegistry,
        prepared_record_meta: Option<crate::thread_store::SessionMeta>,
        runtime_thread_id: Option<String>,
    ) -> io::Result<Self> {
        let session = InteractiveSession::new_with_prepared_history_and_runtime_id(
            config,
            &title.into(),
            preloaded,
            mcp_registry,
            prepared_record_meta,
            runtime_thread_id.clone(),
        )?;
        Ok(Self::from_session(session, runtime_thread_id))
    }

    fn from_session(session: InteractiveSession, runtime_thread_id: Option<String>) -> Self {
        let thread_id = runtime_thread_id.unwrap_or_else(|| {
            session
                .session_id()
                .map(ToString::to_string)
                .unwrap_or_else(new_run_id)
        });
        let mut lifecycle = RuntimeSessionLifecycle::new(thread_id.clone());
        lifecycle.start_task(RuntimeTaskKind::Agent);

        Self {
            thread_extensions: Arc::new(ExtensionData::new(thread_id.clone())),
            thread_id,
            session,
            lifecycle,
            next_extension_turn: 0,
            goal_runtime: None,
            goal_actor_join: None,
        }
    }

    pub(crate) fn begin_goal_turn(
        &mut self,
        request: &ThreadTurnRequest,
    ) -> io::Result<Option<GoalRuntimeBinding>> {
        if request.tool_mode() != crate::controller::ThreadTurnToolMode::Goal {
            return Ok(None);
        }
        let Some(session_id) = self.session().session_id().map(str::to_string) else {
            return Ok(None);
        };
        let handle = match self.goal_runtime_handle() {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        };
        let origin = request
            .goal_turn_origin()
            .unwrap_or(orca_core::goal_runtime::GoalTurnOrigin::User);
        let turn = handle
            .begin_outer_turn(
                &session_id,
                origin,
                request.turn_id().to_string(),
                now_timestamp(),
            )
            .map_err(io::Error::other)?;
        let binding = GoalRuntimeBinding {
            handle,
            turn: Some(turn),
        };
        self.thread_extensions.insert(binding.clone());
        Ok(Some(binding))
    }

    pub(crate) fn goal_runtime_handle(&mut self) -> io::Result<GoalRuntimeHandle> {
        if self.goal_runtime.is_none() {
            let (handle, join) = GoalRuntimeHandle::open_default().map_err(io::Error::other)?;
            self.install_goal_runtime(handle, join)?;
        }
        Ok(self
            .goal_runtime
            .as_ref()
            .expect("goal runtime initialized")
            .clone())
    }

    pub(crate) fn initialized_goal_runtime_handle(&self) -> Option<GoalRuntimeHandle> {
        self.goal_runtime.clone()
    }

    pub(crate) fn install_goal_runtime(
        &mut self,
        handle: GoalRuntimeHandle,
        join: std::thread::JoinHandle<()>,
    ) -> io::Result<()> {
        if self.goal_runtime.is_some() || self.goal_actor_join.is_some() {
            let _ = handle.shutdown();
            let _ = join.join();
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "goal runtime is already initialized",
            ));
        }
        self.goal_runtime = Some(handle);
        self.goal_actor_join = Some(join);
        Ok(())
    }

    pub(crate) fn bind_surface_goal_turn(
        &mut self,
        handle: GoalRuntimeHandle,
        turn: crate::goal_actor::GoalTurnContext,
    ) -> GoalRuntimeBinding {
        let binding = GoalRuntimeBinding {
            handle,
            turn: Some(turn),
        };
        self.thread_extensions.insert(binding.clone());
        binding
    }

    pub(crate) fn clear_goal_turn_binding(&mut self) {
        self.thread_extensions.remove::<GoalRuntimeBinding>();
    }

    pub(crate) fn finish_goal_turn(
        &mut self,
        binding: Option<&GoalRuntimeBinding>,
        status: RunStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        usage: orca_core::goal_runtime::GoalUsage,
        mut events: Option<&mut EventFactory>,
        observer: Option<&dyn orca_core::event_sink::EventObserver>,
        evidence: TurnProgressEvidence,
        config: &RunConfig,
        cancel: CancelToken,
    ) -> io::Result<()> {
        let Some(binding) = binding else {
            return Ok(());
        };
        let Some(turn) = binding.turn.as_ref() else {
            self.thread_extensions.remove::<GoalRuntimeBinding>();
            return Ok(());
        };
        let goal_status = match status {
            RunStatus::Success => orca_core::goal_runtime::GoalTurnStatus::Success,
            RunStatus::Cancelled => orca_core::goal_runtime::GoalTurnStatus::Cancelled,
            RunStatus::ApprovalRequired => {
                orca_core::goal_runtime::GoalTurnStatus::ApprovalRequired
            }
            RunStatus::BudgetExhausted => orca_core::goal_runtime::GoalTurnStatus::BudgetExhausted,
            RunStatus::Failed | RunStatus::VerificationFailed => {
                orca_core::goal_runtime::GoalTurnStatus::Failed
            }
        };
        let previous_state = binding
            .handle
            .read(&turn.session_id)
            .ok()
            .and_then(|record| record.map(|record| record.state));
        let action = binding.handle.finish_outer_turn_with_progress(
            &turn.session_id,
            goal_status,
            end_reason,
            usage.clone(),
            evidence.tool_count,
            evidence.model_response_count,
            evidence.has_substantive_progress(),
            (!evidence.has_substantive_progress())
                .then(|| crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT.to_string()),
            now_timestamp(),
        );
        // A failed settlement leaves the outer turn in flight. Surfacing it lets
        // the caller keep the binding for the supervisor-side settler instead of
        // silently stranding the Goal as active with an unfinished turn.
        let action: Result<_, crate::goal_actor::GoalActorError> = match action {
            Ok(action) => Ok(action),
            Err(error) => {
                if let Some(events) = events.as_deref_mut() {
                    let _ = orca_core::event_sink::observe_event(
                        observer,
                        events.goal_turn_finished(
                            &turn.outer_turn_id,
                            goal_status,
                            &usage,
                            &orca_core::goal_runtime::GoalNextAction::Pause {
                                reason: orca_core::goal_runtime::GoalPauseReason::Infrastructure,
                                message: error.to_string(),
                            },
                        ),
                    );
                }
                return Err(io::Error::other(format!(
                    "goal outer turn settlement failed: {error}"
                )));
            }
        };
        let mut final_action = action.clone().ok();
        if let Ok(orca_core::goal_runtime::GoalNextAction::Verify { intent }) = action {
            let record = binding.handle.read(&turn.session_id).ok().flatten();
            let mut verification_request = GoalVerificationRequest::new(
                record
                    .as_ref()
                    .map(|record| record.objective.clone())
                    .unwrap_or_default(),
                intent,
            );
            if let Some(record) = record.as_ref() {
                verification_request.goal_state = record.state.clone();
                verification_request.budget_remaining = record
                    .token_budget
                    .map(|budget| budget.saturating_sub(record.usage.charged_tokens()));
            }
            verification_request.active_workflow = self.session.has_active_workflows();
            verification_request.last_model_response =
                self.session.conversation().messages.iter().rev().find_map(
                    |message| match message {
                        orca_core::conversation::Message::Assistant { content, .. } => {
                            content.clone()
                        }
                        _ => None,
                    },
                );
            let verifier: Box<dyn GoalVerifier> =
                if config.provider == orca_core::config::ProviderKind::DeepSeek {
                    Box::new(DeepSeekGoalVerifier::new(
                        orca_provider::ProviderConfig {
                            api_key: config.api_key.clone(),
                            base_url: config.base_url.clone(),
                            model: config.model.as_option(),
                            reasoning_effort: config.reasoning_effort,
                            tools_override: Some(Vec::new()),
                            mcp_registry: None,
                            external_tools: Vec::new(),
                        },
                        cancel,
                    ))
                } else {
                    Box::new(DeterministicGoalVerifier)
                };
            match verifier.verify(&verification_request) {
                Ok(output) => {
                    if output.usage.charged_tokens() > 0
                        || output.usage.cost_micros > 0
                        || output.usage.elapsed_seconds > 0
                    {
                        let _ = binding.handle.record_verifier_usage_once(
                            &turn.outer_turn_id,
                            GoalUsageEvent {
                                usage_event_id: format!("verifier:{}:1", turn.outer_turn_id),
                                goal_id: turn.goal_id.clone(),
                                source: "goal_verifier".to_string(),
                                usage: output.usage.clone(),
                                created_at: now_timestamp(),
                            },
                        );
                    }
                    if let Some(events) = events.as_deref_mut() {
                        let _ = orca_core::event_sink::observe_event(
                            observer,
                            events.goal_verification_completed(&turn.outer_turn_id, &output.result),
                        );
                    }
                    let _ = binding
                        .handle
                        .verify(&turn.session_id, output.result, now_timestamp())
                        .map(|action| final_action = Some(action));
                }
                Err(error) => {
                    if let Some(events) = events.as_deref_mut() {
                        let result =
                            orca_core::goal_runtime::GoalVerificationResult::Indeterminate {
                                message: error.to_string(),
                            };
                        let _ = orca_core::event_sink::observe_event(
                            observer,
                            events.goal_verification_completed(&turn.outer_turn_id, &result),
                        );
                    }
                    let _ = binding.handle.pause(
                        &turn.session_id,
                        orca_core::goal_runtime::GoalPauseReason::Infrastructure,
                        error.to_string(),
                        now_timestamp(),
                    );
                    final_action = Some(orca_core::goal_runtime::GoalNextAction::Pause {
                        reason: orca_core::goal_runtime::GoalPauseReason::Infrastructure,
                        message: error.to_string(),
                    });
                }
            }
        }
        if let (Some(events), Some(next_action)) = (events.as_deref_mut(), final_action.as_ref()) {
            let _ = orca_core::event_sink::observe_event(
                observer,
                events.goal_turn_finished(&turn.outer_turn_id, goal_status, &usage, next_action),
            );
        }
        if let (Some(events), Some(previous_state)) = (events.as_deref_mut(), previous_state) {
            if let Ok(Some(record)) = binding.handle.read(&turn.session_id)
                && record.state != previous_state
            {
                let _ = orca_core::event_sink::observe_event(
                    observer,
                    events.goal_transitioned(
                        &turn.goal_id,
                        &previous_state,
                        &record.state,
                        record
                            .last_transition
                            .as_ref()
                            .map(|transition| transition.reason_code.as_str())
                            .unwrap_or("runtime"),
                    ),
                );
                if let orca_core::goal_runtime::GoalState::Complete { evidence } = &record.state {
                    let _ = orca_core::event_sink::observe_event(
                        observer,
                        events.goal_completed(
                            &turn.goal_id,
                            Some(&turn.goal_run_id),
                            evidence,
                            &record.usage,
                        ),
                    );
                }
            }
        }
        self.thread_extensions.remove::<GoalRuntimeBinding>();
        Ok(())
    }

    pub(crate) fn emit_goal_turn_started(
        binding: Option<&GoalRuntimeBinding>,
        events: &mut EventFactory,
        observer: Option<&dyn orca_core::event_sink::EventObserver>,
    ) {
        let Some(turn) = binding.and_then(|binding| binding.turn.as_ref()) else {
            return;
        };
        if turn.run_started {
            let _ = orca_core::event_sink::observe_event(
                observer,
                events.goal_run_started(&turn.goal_id, &turn.goal_run_id),
            );
        }
        let _ = orca_core::event_sink::observe_event(
            observer,
            events.goal_turn_started(
                &turn.goal_id,
                &turn.goal_run_id,
                &turn.outer_turn_id,
                turn.origin,
            ),
        );
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn session(&self) -> &InteractiveSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut InteractiveSession {
        &mut self.session
    }

    pub fn lifecycle(&self) -> &RuntimeSessionLifecycle {
        &self.lifecycle
    }

    pub fn lifecycle_mut(&mut self) -> &mut RuntimeSessionLifecycle {
        &mut self.lifecycle
    }

    pub fn thread_extensions(&self) -> &ExtensionData {
        self.thread_extensions.as_ref()
    }

    pub fn thread_extensions_handle(&self) -> Arc<ExtensionData> {
        Arc::clone(&self.thread_extensions)
    }

    pub(crate) fn event_factory(&self) -> EventFactory {
        let run_id = self.thread_id.clone();
        let Some((next_seq, writer)) = self.session.event_publication_store() else {
            return EventFactory::new(run_id);
        };
        let store: Arc<dyn EventPublicationStore> = Arc::new(writer);
        EventFactory::with_publication_store(run_id, next_seq, store)
    }

    pub fn run_turn_to_writer<W: io::Write>(
        &mut self,
        config: &RunConfig,
        prompt: &str,
        writer: W,
        options: ControllerRunOptions,
    ) -> io::Result<RunStatus> {
        self.run_request(
            config,
            &ThreadTurnRequest::new(prompt).with_options(options),
            writer,
        )
    }

    pub fn run_request<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let usage_before = self.session.aggregate_usage_totals();
        let progress_baseline = TurnProgressBaseline::capture(self.session.conversation());
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request(request, writer);
        let evidence =
            turn_progress_evidence_since(self.session.conversation(), &progress_baseline);
        let settled = self.finish_goal_turn(
            binding.as_ref(),
            result.as_ref().copied().unwrap_or(RunStatus::Failed),
            crate::lifecycle::TurnEndReason::Unclassified,
            goal_usage_delta(usage_before, self.session.aggregate_usage_totals()),
            None,
            None,
            evidence,
            config,
            CancelToken::new(),
        );
        result.and_then(|status| settled.map(|()| status))
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let verifier_cancel = cancel.clone();
        let usage_before = self.session.aggregate_usage_totals();
        let progress_baseline = TurnProgressBaseline::capture(self.session.conversation());
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_cancel(request, writer, cancel);
        let evidence =
            turn_progress_evidence_since(self.session.conversation(), &progress_baseline);
        let settled = self.finish_goal_turn(
            binding.as_ref(),
            result.as_ref().copied().unwrap_or(RunStatus::Failed),
            crate::lifecycle::TurnEndReason::Unclassified,
            goal_usage_delta(usage_before, self.session.aggregate_usage_totals()),
            None,
            None,
            evidence,
            config,
            verifier_cancel,
        );
        result.and_then(|status| settled.map(|()| status))
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        self.run_request_with_event_factory_and_cancel(
            config,
            request,
            writer,
            events,
            CancelToken::new(),
        )
    }

    pub fn run_request_with_event_factory_and_cancel<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        let binding = self.begin_goal_turn(request)?;
        let observer = request.event_observer().map(Arc::as_ref);
        Self::emit_goal_turn_started(binding.as_ref(), events, observer);
        let verifier_cancel = cancel.clone();
        let usage_before = self.session.aggregate_usage_totals();
        let progress_baseline = TurnProgressBaseline::capture(self.session.conversation());
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        let result = ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_event_factory_and_cancel(request, writer, events, cancel);
        let evidence =
            turn_progress_evidence_since(self.session.conversation(), &progress_baseline);
        let settled = self.finish_goal_turn(
            binding.as_ref(),
            result.as_ref().copied().unwrap_or(RunStatus::Failed),
            crate::lifecycle::TurnEndReason::Unclassified,
            goal_usage_delta(usage_before, self.session.aggregate_usage_totals()),
            Some(events),
            observer,
            evidence,
            config,
            verifier_cancel,
        );
        result.and_then(|status| settled.map(|()| status))
    }

    pub(crate) fn run_request_with_event_factory_and_cancel_outcome_unbound<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<ThreadTurnOutcome> {
        let thread_extensions = self.thread_extensions_handle();
        let turn_extension_id = self.next_turn_extension_id();
        ThreadTurnExecutor::new_with_thread_extensions(
            config,
            &mut self.session,
            &mut self.lifecycle,
            thread_extensions,
            turn_extension_id,
        )
        .run_request_with_event_factory_and_cancel_outcome(request, writer, events, cancel)
    }

    pub fn run_request_with_event_factory_and_cancel_outcome<W: io::Write>(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
        cancel: CancelToken,
    ) -> io::Result<ThreadTurnOutcome> {
        let binding = self.begin_goal_turn(request)?;
        let observer = request.event_observer().map(Arc::as_ref);
        Self::emit_goal_turn_started(binding.as_ref(), events, observer);
        let verifier_cancel = cancel.clone();
        let usage_before = self.session.aggregate_usage_totals();
        let progress_baseline = TurnProgressBaseline::capture(self.session.conversation());
        let result = self.run_request_with_event_factory_and_cancel_outcome_unbound(
            config, request, writer, events, cancel,
        );
        let evidence =
            turn_progress_evidence_since(self.session.conversation(), &progress_baseline);
        let settled = self.finish_goal_turn(
            binding.as_ref(),
            match &result {
                Ok(ThreadTurnOutcome::Completed { status, .. }) => *status,
                Ok(ThreadTurnOutcome::ProviderSuspended { .. }) => RunStatus::ApprovalRequired,
                Err(_) => RunStatus::Failed,
            },
            match &result {
                Ok(ThreadTurnOutcome::Completed { end_reason, .. }) => *end_reason,
                Ok(ThreadTurnOutcome::ProviderSuspended { .. }) => {
                    crate::lifecycle::TurnEndReason::Unclassified
                }
                Err(_) => crate::lifecycle::TurnEndReason::Unclassified,
            },
            goal_usage_delta(usage_before, self.session.aggregate_usage_totals()),
            Some(events),
            observer,
            evidence,
            config,
            verifier_cancel,
        );
        result.and_then(|outcome| settled.map(|()| outcome))
    }

    fn next_turn_extension_id(&mut self) -> String {
        self.next_extension_turn = self.next_extension_turn.saturating_add(1);
        format!("{}:turn-{}", self.thread_id, self.next_extension_turn)
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        if let Some(handle) = self.goal_runtime.as_ref() {
            let _ = handle.shutdown();
        }
        if let Some(join) = self.goal_actor_join.take() {
            let _ = join.join();
        }
    }
}

fn now_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(crate) fn goal_usage_delta(
    before: UsageTotals,
    after: UsageTotals,
) -> orca_core::goal_runtime::GoalUsage {
    let cost_delta = (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0);
    let cost_micros = i64::try_from(crate::cost::usd_to_micros(cost_delta)).unwrap_or(i64::MAX);
    orca_core::goal_runtime::GoalUsage {
        charged_input_tokens: after.input_tokens.saturating_sub(before.input_tokens) as i64,
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens) as i64,
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens) as i64,
        cost_micros,
        ..orca_core::goal_runtime::GoalUsage::default()
    }
}

pub(crate) fn plan_snapshot(conversation: &orca_core::conversation::Conversation) -> Option<&str> {
    conversation
        .internal_context
        .get(orca_core::conversation::PLAN_CONTEXT_FRAGMENT_ID)
        .map(|fragment| fragment.content.as_str())
}

/// Measures activity added after the baseline and separates completed
/// side-effecting tool calls / plan transitions from read-only exploration.
/// Compaction can shrink the message log mid-turn; in that case message counts
/// safely fall back to zero while an independently changed plan still counts.
pub(crate) fn turn_progress_evidence_since(
    conversation: &orca_core::conversation::Conversation,
    baseline: &TurnProgressBaseline,
) -> TurnProgressEvidence {
    use std::collections::HashSet;

    use orca_core::tool_types::{ToolName, ToolStatus};

    let added = conversation
        .messages
        .get(baseline.message_count..)
        .unwrap_or_default();
    let completed_tool_calls = added
        .iter()
        .filter_map(|message| match message {
            orca_core::conversation::Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if terminal.status == ToolStatus::Completed => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut evidence = TurnProgressEvidence {
        plan_changed: baseline.plan_snapshot.as_deref() != plan_snapshot(conversation),
        ..TurnProgressEvidence::default()
    };
    for message in added {
        match message {
            orca_core::conversation::Message::Assistant { tool_calls, .. } => {
                evidence.model_response_count = evidence.model_response_count.saturating_add(1);
                for tool_call in tool_calls {
                    if completed_tool_calls.contains(tool_call.id.as_str())
                        && ToolName::from_str(&tool_call.function_name)
                            .is_none_or(|name| !name.is_read_only())
                    {
                        evidence.substantive_tool_count =
                            evidence.substantive_tool_count.saturating_add(1);
                    }
                }
            }
            orca_core::conversation::Message::Tool { .. } => {
                evidence.tool_count = evidence.tool_count.saturating_add(1);
            }
            _ => {}
        }
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::CostTracker;
    use crate::lifecycle::RuntimeTurnState;
    use crate::tasks::TaskRegistry;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_config(cwd: PathBuf) -> RunConfig {
        // Every test resolves ORCA_HOME to the process-wide isolated home so
        // parallel tests never contend with live `orca` processes or each
        // other's deleted temp dirs; an explicitly provided home (recovery
        // child fixture) is preserved.
        let _ = crate::history::claim_isolated_test_orca_home_if_unset();
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: HashMap::new(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn turn_progress_evidence_reports_activity() {
        let empty = TurnProgressEvidence::default();
        assert_eq!(empty.tool_count, 0);
        assert_eq!(empty.model_response_count, 0);
        assert_eq!(empty.substantive_tool_count, 0);
        assert!(!empty.plan_changed);
        assert!(!empty.has_activity());
        assert!(!empty.has_substantive_progress());

        let active = TurnProgressEvidence {
            tool_count: 3,
            model_response_count: 1,
            substantive_tool_count: 1,
            plan_changed: false,
        };
        assert!(active.has_activity());
        assert!(active.has_substantive_progress());

        let responses_only = TurnProgressEvidence {
            tool_count: 0,
            model_response_count: 2,
            substantive_tool_count: 0,
            plan_changed: false,
        };
        assert!(responses_only.has_activity());
        assert!(!responses_only.has_substantive_progress());

        let plan_only = TurnProgressEvidence {
            plan_changed: true,
            ..TurnProgressEvidence::default()
        };
        assert!(plan_only.has_substantive_progress());
    }

    #[test]
    fn goal_usage_delta_rounds_cost_micros_like_surface_projection() {
        let usage = goal_usage_delta(
            UsageTotals::default(),
            UsageTotals {
                input_tokens: 7,
                output_tokens: 3,
                cache_tokens: 0,
                estimated_cost_usd: 0.000_001_5,
            },
        );

        assert_eq!(usage.charged_input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cost_micros, 2);
    }

    #[test]
    fn turn_progress_evidence_distinguishes_reads_from_completed_side_effects() {
        use orca_core::approval_types::ActionKind;
        use orca_core::conversation::{Conversation, RawToolCall};
        use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};

        let mut conversation = Conversation::new();
        conversation
            .messages
            .push(orca_core::conversation::Message::User {
                content: "before".to_string(),
                pinned: false,
            });
        let baseline = TurnProgressBaseline::capture(&conversation);
        let read = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("src/lib.rs".to_string()),
            raw_arguments: None,
        };
        let edit = ToolRequest {
            id: "edit-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("src/lib.rs".to_string()),
            raw_arguments: None,
        };
        conversation.add_assistant(
            Some("inspect then edit".to_string()),
            None,
            vec![
                RawToolCall {
                    id: read.id.clone(),
                    function_name: read.name.as_str().to_string(),
                    arguments: "{}".to_string(),
                },
                RawToolCall {
                    id: edit.id.clone(),
                    function_name: edit.name.as_str().to_string(),
                    arguments: "{}".to_string(),
                },
            ],
        );
        conversation.add_tool_result_with_terminal(
            &ToolResult::completed(&read, "contents".to_string(), false),
            "contents".to_string(),
        );
        conversation.add_tool_result_with_terminal(
            &ToolResult::completed(&edit, "edited".to_string(), false),
            "edited".to_string(),
        );

        let evidence = turn_progress_evidence_since(&conversation, &baseline);
        assert_eq!(evidence.model_response_count, 1);
        assert_eq!(evidence.tool_count, 2);
        assert_eq!(evidence.substantive_tool_count, 1);
        assert!(!evidence.plan_changed);
        assert!(evidence.has_activity());
        assert!(evidence.has_substantive_progress());

        // A conversation shrunk by compaction must not panic or overcount.
        let shrunk = turn_progress_evidence_since(
            &conversation,
            &TurnProgressBaseline {
                message_count: 99,
                plan_snapshot: None,
            },
        );
        assert_eq!(shrunk, TurnProgressEvidence::default());
    }

    #[test]
    fn turn_progress_evidence_treats_plan_change_as_progress() {
        use orca_core::conversation::Conversation;

        let mut conversation = Conversation::new();
        conversation.replace_plan_state("[pending] inspect runtime".to_string());
        let baseline = TurnProgressBaseline::capture(&conversation);
        conversation
            .replace_plan_state("[completed] inspect runtime\n[in_progress] fix gate".to_string());

        let evidence = turn_progress_evidence_since(&conversation, &baseline);
        assert!(evidence.plan_changed);
        assert!(evidence.has_substantive_progress());
    }

    #[test]
    fn runtime_thread_starts_with_runtime_owned_session_and_lifecycle() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());

        let thread = RuntimeThread::start(&config, "inspect repo").unwrap();

        assert!(thread.thread_id().starts_with("run-"));
        assert_eq!(thread.session().conversation().messages.len(), 1);
        assert_eq!(thread.lifecycle().run_id(), thread.thread_id());
    }

    #[test]
    fn runtime_thread_exposes_session_mutation_through_boundary() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());
        let mut thread = RuntimeThread::start(&config, "inspect repo").unwrap();

        thread
            .session_mut()
            .replace_skill_context(Some("thread skill marker".to_string()));

        let skill_context = thread
            .session()
            .conversation()
            .internal_context
            .get(orca_core::conversation::SKILL_CONTEXT_FRAGMENT_ID)
            .map(|fragment| fragment.content.as_str())
            .unwrap_or_default();
        assert!(skill_context.contains("thread skill marker"));
    }

    #[derive(Debug)]
    struct ThreadExtensionMarker(&'static str);

    #[derive(Debug)]
    struct TurnExtensionMarker(&'static str);

    #[test]
    fn runtime_thread_reuses_thread_extensions_across_turn_states() {
        let cwd = tempfile::tempdir().unwrap();
        let config = test_config(cwd.path().to_path_buf());
        let mut thread = RuntimeThread::start(&config, "inspect repo").unwrap();
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new(thread.thread_id().to_string());
        let first_turn_id = thread.next_turn_extension_id();
        let second_turn_id = thread.next_turn_extension_id();

        assert_eq!(thread.thread_extensions().level_id(), thread.thread_id());
        assert_eq!(first_turn_id, format!("{}:turn-1", thread.thread_id()));
        assert_eq!(second_turn_id, format!("{}:turn-2", thread.thread_id()));
        thread
            .thread_extensions()
            .insert(ThreadExtensionMarker("thread-scoped"));

        {
            let mut cost_tracker = CostTracker::new(None);
            let first_turn_state = RuntimeTurnState::new_with_thread_extensions(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                thread.thread_extensions_handle(),
                first_turn_id,
            );

            first_turn_state
                .turn_extensions()
                .insert(TurnExtensionMarker("turn-scoped"));
            assert_eq!(
                first_turn_state
                    .thread_extensions()
                    .get::<ThreadExtensionMarker>()
                    .expect("thread marker should persist")
                    .0,
                "thread-scoped"
            );
            assert_eq!(
                first_turn_state
                    .turn_extensions()
                    .get::<TurnExtensionMarker>()
                    .expect("turn marker should exist in first turn")
                    .0,
                "turn-scoped"
            );
        }

        let mut cost_tracker = CostTracker::new(None);
        let second_turn_state = RuntimeTurnState::new_with_thread_extensions(
            &mut cost_tracker,
            &cancel,
            &task_registry,
            thread.thread_extensions_handle(),
            second_turn_id.clone(),
        );

        assert_eq!(
            second_turn_state.turn_extensions().level_id(),
            second_turn_id
        );
        assert_eq!(
            second_turn_state
                .thread_extensions()
                .get::<ThreadExtensionMarker>()
                .expect("thread marker should survive the next turn")
                .0,
            "thread-scoped"
        );
        assert!(
            second_turn_state
                .turn_extensions()
                .get::<TurnExtensionMarker>()
                .is_none(),
            "turn-scoped marker must not leak into later turns"
        );
    }

    fn with_orca_home<T>(f: impl FnOnce() -> T) -> T {
        // ORCA_HOME stays on the process-wide isolated home (never removed)
        // so concurrent tests always resolve a live directory; the env lock
        // serializes tests that deliberately share config/SQLite state.
        let _guard = crate::history::lock_test_env();
        let _ = crate::history::isolated_test_orca_home();
        f()
    }

    #[test]
    fn finishing_a_goal_turn_surfaces_a_failed_settlement() {
        with_orca_home(|| {
            let cwd = tempfile::tempdir().unwrap();
            let mut config = test_config(cwd.path().to_path_buf());
            config.history_mode = HistoryMode::Record;
            let mut thread = RuntimeThread::start(&config, "settlement failure").unwrap();
            let session_id = thread
                .session()
                .session_id()
                .expect("recorded thread has a session")
                .to_string();
            let handle = thread.goal_runtime_handle().unwrap();
            handle
                .create(crate::goal_store::CreateGoalInput {
                    session_id: session_id.clone(),
                    objective: "surface settlement failures".to_string(),
                    token_budget: None,
                    now: 1,
                })
                .unwrap();
            let turn = handle
                .begin_outer_turn(
                    &session_id,
                    orca_core::goal_runtime::GoalTurnOrigin::User,
                    "turn-1".to_string(),
                    2,
                )
                .unwrap();
            let binding = GoalRuntimeBinding {
                handle,
                turn: Some(turn),
            };
            let evidence = TurnProgressEvidence::default();

            thread
                .finish_goal_turn(
                    Some(&binding),
                    RunStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    orca_core::goal_runtime::GoalUsage::default(),
                    None,
                    None,
                    evidence,
                    &config,
                    CancelToken::new(),
                )
                .expect("first settlement succeeds");

            // The turn is already settled, so the actor has no active outer turn
            // left to finish. That failure must reach the caller rather than
            // being dropped, which would strand the Goal as active in flight.
            let error = thread
                .finish_goal_turn(
                    Some(&binding),
                    RunStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    orca_core::goal_runtime::GoalUsage::default(),
                    None,
                    None,
                    evidence,
                    &config,
                    CancelToken::new(),
                )
                .expect_err("a failed settlement must not be swallowed");
            assert!(
                error.to_string().contains("settlement failed"),
                "unexpected settlement error: {error}"
            );
        });
    }
}
