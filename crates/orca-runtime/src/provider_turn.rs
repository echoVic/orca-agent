use std::io;
#[cfg(test)]
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
#[cfg(test)]
use orca_core::subagent_types::SubagentType;
use orca_core::thread_item_projection::ModelResponseIdentity;
#[cfg(test)]
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, ProviderStreamEvent, context};

use crate::agent_child::ChildAgentExecutor;
use crate::background_turn::RuntimeTurnContinuation;
use crate::compaction::{
    RuntimeCompactionPolicy, RuntimeCompactionRetryDecision, RuntimeCompactionRetryState,
    RuntimeCompactionStep,
};
use crate::cost::CostTracker;
use crate::extension::RuntimeExtensionContext;
use crate::hooks::{HookRunner, conversation_with_hook_context};
#[cfg(test)]
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnContext, RuntimeTurnStartError, TurnEndReason,
};
use crate::memory;
#[cfg(test)]
use crate::memory::MemoryBlock;
use crate::model_response::RuntimeModelResponse;
use crate::provider_stream::RuntimeProviderSuspension;
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_directive::conversation_with_runtime_system_messages;
use crate::runtime_steer::{RuntimeSteerInput, RuntimeSteerStep};
use crate::runtime_turn_kernel::RuntimeTurnKernel;
use crate::session::record_assistant_response_for_agent;
use crate::step_context::{
    RuntimeSamplingRequestState, RuntimeStepCapabilitySnapshot, RuntimeStepContext,
    RuntimeStepSnapshot,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{AgentToolPolicyContext, tool_requests_from_provider_steps};
use crate::tool_turn::{
    RuntimeToolTurnsContext, RuntimeToolTurnsExecutors, RuntimeToolTurnsIo, ToolTurnOutcome,
    run_tool_turns,
};
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeProviderErrorResultStep;
pub(crate) struct RuntimeProviderErrorStep {
    compaction_retry: RuntimeCompactionRetryState,
}
pub(crate) struct RuntimeProviderTurnResultStep;
pub(crate) struct RuntimeProviderTurnResultResultStep;

pub(crate) struct RuntimeProviderTurnStep;
pub(crate) struct RuntimeProviderResponseStep;
pub(crate) struct RuntimeProviderResponseResultStep;
pub(crate) struct RuntimeTurnProviderCycleStep {
    provider_error_step: RuntimeProviderErrorStep,
}

pub(crate) struct RuntimeProviderCycleInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) continuation: Option<RuntimeTurnContinuation>,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) turn_provider_config: &'a ProviderConfig,
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) base_provider_config: &'a ProviderConfig,
    pub(crate) capabilities: RuntimeStepCapabilitySnapshot<'a>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) extensions: RuntimeExtensionContext<'a>,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeProviderTurnInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) max_budget_usd: Option<f64>,
    /// Full model context window, used as the denominator for the local
    /// `context.updated` observability event emitted after real usage arrives.
    pub(crate) context_window: usize,
    pub(crate) io: RuntimeProviderTurnIo<'a, W>,
}

pub(crate) struct RuntimeProviderTurnIo<'a, W: io::Write> {
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
}

pub(crate) struct RuntimeProviderResponseInput<'a, W: io::Write> {
    pub(crate) step_context: RuntimeStepContext<'a>,
    pub(crate) sampling_state: &'a mut RuntimeSamplingRequestState,
    pub(crate) io: RuntimeProviderResponseIo<'a, W>,
}

pub(crate) struct RuntimeProviderResponseIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeProviderResponseExecutors {
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) enum RuntimeProviderTurnOutput {
    Response(RuntimeModelResponse),
    Terminal(RuntimeTurnStartError),
    Suspended(RuntimeProviderSuspension),
}

pub(crate) enum RuntimeProviderErrorOutcome {
    ContinueAfterCompaction,
    Failed(String),
    NoError,
}

pub(crate) enum RuntimeProviderResponseOutcome {
    Continue,
    Success {
        final_message: Option<String>,
    },
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) enum RuntimeProviderErrorStepOutcome {
    ContinueAfterCompaction,
    Failed(RuntimeTurnStartError),
    NoError,
}

pub(crate) enum RuntimeProviderErrorResult {
    ContinueTurn,
    ContinueLoop,
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeProviderTurnResultOutcome {
    Response(RuntimeModelResponse),
    Failed(RuntimeTurnStartError),
    Suspended(RuntimeProviderSuspension),
}

pub(crate) enum RuntimeProviderTurnResultResult {
    Response(RuntimeModelResponse),
    Return(AgentLoopResult),
    Suspended(RuntimeProviderSuspension),
}

pub(crate) enum RuntimeProviderResponseResult {
    Continue,
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeTurnProviderCycleResult {
    ContinueLoop,
    ContinueTurn,
    Return(AgentLoopResult),
    Suspended(RuntimeProviderSuspension),
}

impl RuntimeProviderErrorResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderErrorStepOutcome,
    ) -> RuntimeProviderErrorResult {
        match outcome {
            RuntimeProviderErrorStepOutcome::NoError => RuntimeProviderErrorResult::ContinueTurn,
            RuntimeProviderErrorStepOutcome::ContinueAfterCompaction => {
                RuntimeProviderErrorResult::ContinueLoop
            }
            RuntimeProviderErrorStepOutcome::Failed(error) => {
                RuntimeProviderErrorResult::Return(AgentLoopResult::from(error))
            }
        }
    }
}

impl RuntimeProviderErrorStep {
    pub(crate) fn new() -> Self {
        Self {
            compaction_retry: RuntimeCompactionRetryState::default(),
        }
    }

    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
    ) -> io::Result<RuntimeProviderErrorStepOutcome> {
        match RuntimeProviderTurnStep::new().handle_provider_error(
            response,
            compaction,
            conversation,
            &self.compaction_retry,
        )? {
            RuntimeProviderErrorOutcome::ContinueAfterCompaction => {
                self.compaction_retry.record_prompt_too_long_retry();
                Ok(RuntimeProviderErrorStepOutcome::ContinueAfterCompaction)
            }
            RuntimeProviderErrorOutcome::Failed(message) => {
                self.compaction_retry.reset();
                Ok(RuntimeProviderErrorStepOutcome::Failed(
                    RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        reason: TurnEndReason::Unclassified,
                        message,
                    },
                ))
            }
            RuntimeProviderErrorOutcome::NoError => {
                self.compaction_retry.reset();
                Ok(RuntimeProviderErrorStepOutcome::NoError)
            }
        }
    }

    #[cfg(test)]
    fn mark_reactive_compacted_for_test(&mut self) {
        self.compaction_retry.record_prompt_too_long_retry();
    }

    #[cfg(test)]
    fn reactive_compacted_for_test(&self) -> bool {
        self.compaction_retry.has_prompt_too_long_retry()
    }
}

impl RuntimeProviderTurnResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold<W: io::Write>(
        &mut self,
        provider_turn: RuntimeProviderTurnOutput,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        emit_deltas: bool,
    ) -> io::Result<RuntimeProviderTurnResultOutcome> {
        match provider_turn {
            RuntimeProviderTurnOutput::Response(response) => {
                Ok(RuntimeProviderTurnResultOutcome::Response(response))
            }
            RuntimeProviderTurnOutput::Terminal(error) => {
                if emit_deltas && error.status != RunStatus::Cancelled {
                    sink.emit(events.error(&error.message))?;
                }
                Ok(RuntimeProviderTurnResultOutcome::Failed(error))
            }
            RuntimeProviderTurnOutput::Suspended(suspension) => {
                Ok(RuntimeProviderTurnResultOutcome::Suspended(suspension))
            }
        }
    }
}

impl RuntimeProviderTurnResultResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderTurnResultOutcome,
    ) -> RuntimeProviderTurnResultResult {
        match outcome {
            RuntimeProviderTurnResultOutcome::Response(response) => {
                RuntimeProviderTurnResultResult::Response(response)
            }
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                RuntimeProviderTurnResultResult::Return(AgentLoopResult::from(error))
            }
            RuntimeProviderTurnResultOutcome::Suspended(suspension) => {
                RuntimeProviderTurnResultResult::Suspended(suspension)
            }
        }
    }
}

impl RuntimeProviderTurnStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        input: RuntimeProviderTurnInput<'_, '_, W>,
    ) -> io::Result<RuntimeProviderTurnOutput> {
        let RuntimeProviderTurnIo {
            conversation,
            cost_tracker,
            events,
            sink,
            mut history_writer,
        } = input.io;
        let cwd_display = input.turn_context.cwd.display().to_string();
        let emit_deltas = input.turn_context.emit_deltas;
        let pre_model_outcome = match input.actor.run_pre_model_hook_with_cancel(
            input.hooks,
            &cwd_display,
            Some(input.cancel),
        ) {
            Ok(outcome) => outcome,
            Err(error) => return Ok(RuntimeProviderTurnOutput::terminal(error)),
        };
        if input.cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        RuntimeSteerStep::new().apply(RuntimeSteerInput {
            turn_context: input.turn_context.clone(),
            conversation,
            history_writer: history_writer.as_deref_mut(),
        })?;
        let model_conversation = conversation_with_hook_context(conversation, &pre_model_outcome);
        let model_conversation = conversation_with_runtime_system_messages(
            &model_conversation,
            input.runtime_system_messages,
        );
        if input.provider == ProviderKind::DeepSeek
            && let Some(writer) = history_writer.as_deref_mut()
        {
            let checkpoint = orca_provider::prompt_cache::checkpoint_for_deepseek_request(
                &model_conversation,
                input.provider_config,
            )
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to calculate DeepSeek prompt-cache checkpoint: {error}"),
                )
            })?;
            writer
                .append_prompt_cache_checkpoint(input.turn_context.turn_id.clone(), checkpoint)?;
        }
        let response_identity = ModelResponseIdentity::new(input.turn_context.turn_id.clone());
        let mut stream = orca_provider::start_streaming(
            input.provider,
            &model_conversation,
            input.provider_config,
            input.cancel.clone(),
        );
        let response = loop {
            match stream.recv_timeout(Duration::from_millis(10)) {
                Ok(ProviderStreamEvent::Step(delivery)) => {
                    emit_provider_delta(
                        delivery.step(),
                        &response_identity,
                        input.turn_context.provider_response_ingress(),
                        emit_deltas,
                        events,
                        sink,
                    )?;
                }
                Ok(ProviderStreamEvent::Completed(response)) => break response,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        reason: TurnEndReason::Unclassified,
                        message: "provider stream disconnected before completion".to_string(),
                    }));
                }
            }
            if input
                .turn_context
                .provider_suspension_control
                .is_some_and(|control| control.take_suspension_request())
            {
                return Ok(RuntimeProviderTurnOutput::Suspended(
                    RuntimeProviderSuspension::new(
                        stream,
                        input.provider_config.model.clone(),
                        response_identity,
                    ),
                ));
            }
        };

        let mut usage_error = None;
        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            let totals = match input
                .actor
                .record_usage(usage, cost_tracker, input.max_budget_usd)
            {
                Ok(totals) => totals,
                Err(error) => {
                    usage_error = Some(error);
                    cost_tracker.totals()
                }
            };
            if emit_deltas {
                sink.emit(events.usage_updated(totals))?;
                if let Some(writer) = history_writer.as_deref_mut() {
                    writer.append_usage(totals)?;
                }
                // Real occupancy: the provider-reported prompt tokens for THIS
                // request (not the cumulative CostTracker totals) against the
                // full model window. Non-persisted observability only.
                sink.emit(
                    events.context_updated(usage.input_tokens as usize, input.context_window),
                )?;
            }
        }

        if input.cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }
        if let Some(error) = usage_error {
            return Ok(RuntimeProviderTurnOutput::terminal(error));
        }

        if let Some(warning) = input.actor.run_post_model_hook_with_cancel(
            input.hooks,
            &cwd_display,
            response.usage.as_ref(),
            Some(input.cancel),
        ) && emit_deltas
        {
            sink.emit(events.error(&warning))?;
        }
        if input.cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        Ok(RuntimeProviderTurnOutput::response(
            RuntimeModelResponse::from_parts(response, response_identity),
        ))
    }

    pub(crate) fn handle_provider_error<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
        compaction_retry: &RuntimeCompactionRetryState,
    ) -> io::Result<RuntimeProviderErrorOutcome> {
        let provider_error = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        });

        let Some(error) = provider_error else {
            return Ok(RuntimeProviderErrorOutcome::NoError);
        };

        match RuntimeCompactionPolicy::decide_for_provider_error(&error, compaction_retry) {
            RuntimeCompactionRetryDecision::CompactAndRetry { trigger, reason: _ } => {
                compaction.compact_after_provider_error_retry(conversation, trigger)?;
                Ok(RuntimeProviderErrorOutcome::ContinueAfterCompaction)
            }
            RuntimeCompactionRetryDecision::SurfaceError => {
                compaction.emit_error(&error)?;
                Ok(RuntimeProviderErrorOutcome::Failed(error))
            }
        }
    }
}

impl RuntimeProviderResponseStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: RuntimeModelResponse,
        input: RuntimeProviderResponseInput<'_, W>,
        executors: RuntimeProviderResponseExecutors,
    ) -> io::Result<RuntimeProviderResponseOutcome> {
        let RuntimeProviderResponseInput {
            step_context,
            sampling_state,
            io,
        } = input;
        let RuntimeProviderResponseIo {
            events,
            sink,
            conversation,
            mut history_writer,
            cost_tracker,
            background_workflows,
        } = io;
        let RuntimeProviderResponseExecutors {
            workflow_child_executor,
            batch_child_executor,
        } = executors;
        let step_snapshot = step_context.snapshot();
        if let Some(ingress) = step_snapshot.turn_context.provider_response_ingress() {
            ingress.commit_response(&response)?;
        }
        let completed_response = response.completed();
        let response = response.response;
        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            record_assistant_response_for_agent(
                conversation,
                &completed_response,
                step_snapshot.turn_context.emit_deltas,
                events,
                sink,
            )?;
            if step_snapshot.turn_context.emit_deltas && step_snapshot.config.auto_memory {
                memory::extract_project_memory_after_final_response(
                    step_snapshot.config,
                    step_snapshot.turn_context.cwd,
                    &conversation.messages,
                    step_snapshot.capabilities.cancel,
                    events,
                    sink,
                )?;
            }
            return Ok(RuntimeProviderResponseOutcome::Success { final_message });
        }

        record_assistant_response_for_agent(
            conversation,
            &completed_response,
            step_snapshot.turn_context.emit_deltas,
            events,
            sink,
        )?;

        let tool_requests = tool_requests_from_provider_steps(&response.steps);
        match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state,
            io: RuntimeToolTurnsIo {
                events,
                sink,
                conversation,
                history_writer: history_writer.as_deref_mut(),
                cost_tracker,
                background_workflows,
            },
            tool_requests: &tool_requests,
            executors: RuntimeToolTurnsExecutors {
                workflow_child_executor,
                batch_child_executor,
            },
        })? {
            ToolTurnOutcome::Continue => Ok(RuntimeProviderResponseOutcome::Continue),
            ToolTurnOutcome::Return { status, error } => {
                Ok(RuntimeProviderResponseOutcome::Return { status, error })
            }
        }
    }
}

impl RuntimeProviderResponseResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderResponseOutcome,
    ) -> RuntimeProviderResponseResult {
        match outcome {
            RuntimeProviderResponseOutcome::Continue => RuntimeProviderResponseResult::Continue,
            RuntimeProviderResponseOutcome::Success { final_message } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::success(final_message))
            }
            RuntimeProviderResponseOutcome::Return { status, error } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::terminal(
                    status,
                    TurnEndReason::Unclassified,
                    error,
                ))
            }
        }
    }
}

impl RuntimeTurnProviderCycleStep {
    pub(crate) fn new() -> Self {
        Self {
            provider_error_step: RuntimeProviderErrorStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeProviderCycleInput<'_, '_, W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnProviderCycleResult> {
        let capabilities = input.capabilities;
        let turn_context = input.turn_context.clone();
        let emit_deltas = turn_context.emit_deltas;
        let continuation = input.continuation.take();
        let preapproved_tool_call_id = continuation
            .as_ref()
            .and_then(RuntimeTurnContinuation::preapproved_tool_call_id)
            .map(str::to_string);
        let response = if let Some(continuation) = continuation {
            continuation.response
        } else {
            let provider_turn = {
                let (conversation, history_writer) = input.conversation.parts_mut();
                RuntimeProviderTurnStep::new().run(RuntimeProviderTurnInput {
                    actor: input.actor,
                    provider: input.provider,
                    runtime_system_messages: input.runtime_system_messages,
                    provider_config: input.turn_provider_config,
                    turn_context: turn_context.clone(),
                    hooks: capabilities.hooks,
                    cancel: capabilities.cancel,
                    max_budget_usd: input.max_budget_usd,
                    context_window: input.context_config.max_tokens,
                    io: RuntimeProviderTurnIo {
                        conversation,
                        cost_tracker: input.cost_tracker,
                        events: input.events,
                        sink: input.sink,
                        history_writer,
                    },
                })?
            };
            match RuntimeProviderTurnResultResultStep::new().fold(
                RuntimeProviderTurnResultStep::new().fold(
                    provider_turn,
                    input.events,
                    input.sink,
                    emit_deltas,
                )?,
            ) {
                RuntimeProviderTurnResultResult::Response(response) => response,
                RuntimeProviderTurnResultResult::Return(result) => {
                    return Ok(RuntimeTurnProviderCycleResult::Return(result));
                }
                RuntimeProviderTurnResultResult::Suspended(suspension) => {
                    return Ok(RuntimeTurnProviderCycleResult::Suspended(suspension));
                }
            }
        };

        let provider_error_outcome = {
            let (conversation, history_writer) = input.conversation.parts_mut();
            self.provider_error_step.handle(
                &response.response,
                &mut RuntimeCompactionStep::new(
                    input.provider,
                    input.context_config,
                    input.base_provider_config,
                    turn_context.clone(),
                    capabilities.hooks,
                    input.events,
                    input.sink,
                    history_writer,
                ),
                conversation,
            )?
        };
        if let RuntimeProviderErrorStepOutcome::Failed(error) = &provider_error_outcome
            && let Some(ingress) = turn_context.provider_response_ingress()
        {
            ingress.commit_provider_failure(&response.identity, &error.message)?;
        }
        if matches!(
            &provider_error_outcome,
            RuntimeProviderErrorStepOutcome::ContinueAfterCompaction
        ) {
            record_provider_retry(
                capabilities.task_registry,
                turn_context.root_task_id,
                input.events,
                input.sink,
            )?;
        }
        match RuntimeProviderErrorResultStep::new().fold(provider_error_outcome) {
            RuntimeProviderErrorResult::ContinueLoop => {
                return Ok(RuntimeTurnProviderCycleResult::ContinueLoop);
            }
            RuntimeProviderErrorResult::Return(result) => {
                return Ok(RuntimeTurnProviderCycleResult::Return(result));
            }
            RuntimeProviderErrorResult::ContinueTurn => {}
        }

        let (conversation, history_writer) = input.conversation.parts_mut();
        let mut kernel = RuntimeTurnKernel::from_extension_stores(input.extensions.stores());
        kernel.set_preapproved_tool_call_id(preapproved_tool_call_id);
        let step_context =
            RuntimeStepContext::from_snapshot(RuntimeStepSnapshot::new_with_capabilities(
                input.config,
                turn_context,
                input.tool_policy,
                input.policy,
                capabilities,
            ));
        let response_input = kernel.provider_response_input(
            step_context,
            input.extensions.registry(),
            input.events,
            input.sink,
            conversation,
            history_writer,
            input.cost_tracker,
            input.background_workflows,
        );
        self.handle_response(
            response,
            response_input,
            RuntimeProviderResponseExecutors {
                workflow_child_executor,
                batch_child_executor,
            },
        )
    }

    pub(crate) fn handle_response<W: io::Write>(
        &mut self,
        response: RuntimeModelResponse,
        input: RuntimeProviderResponseInput<'_, W>,
        executors: RuntimeProviderResponseExecutors,
    ) -> io::Result<RuntimeTurnProviderCycleResult> {
        let provider_response_outcome =
            RuntimeProviderResponseStep::new().handle(response, input, executors)?;

        Ok(
            match RuntimeProviderResponseResultStep::new().fold(provider_response_outcome) {
                RuntimeProviderResponseResult::Continue => {
                    RuntimeTurnProviderCycleResult::ContinueTurn
                }
                RuntimeProviderResponseResult::Return(result) => {
                    RuntimeTurnProviderCycleResult::Return(result)
                }
            },
        )
    }
}

fn record_provider_retry<W: io::Write>(
    task_registry: &TaskRegistry,
    root_task_id: Option<&str>,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<()> {
    let Some(task_id) = root_task_id else {
        return Ok(());
    };
    task_registry
        .record_retry(
            task_id,
            "provider context limit reached; compacted context and retrying",
        )
        .map_err(io::Error::other)?;
    let task = task_registry.summary(task_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("root task '{task_id}' disappeared after recording retry"),
        )
    })?;
    sink.emit(events.task_status_updated(&task))
}

fn cancelled_provider_turn<W: io::Write>(
    _emit_deltas: bool,
    _events: &mut EventFactory,
    _sink: &mut EventSink<W>,
) -> io::Result<RuntimeProviderTurnOutput> {
    Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
        status: RunStatus::Cancelled,
        reason: TurnEndReason::Cancelled,
        message: "turn cancelled".to_string(),
    }))
}

fn emit_provider_delta<W: io::Write>(
    step: &ProviderStep,
    identity: &ModelResponseIdentity,
    semantic_ingress: Option<&dyn crate::runtime_surface::RuntimeProviderResponseIngress>,
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<()> {
    if let Some(ingress) = semantic_ingress {
        ingress.commit_provider_step(identity, step)?;
    }
    if !emit_deltas {
        return Ok(());
    }
    match step {
        ProviderStep::ReasoningDelta(text) => {
            let _ = sink.emit(events.assistant_reasoning_delta(identity, text));
        }
        ProviderStep::MessageDelta(text) => {
            let _ = sink.emit(events.assistant_message_delta(identity, text));
        }
        ProviderStep::ToolCallProgress(progress) => {
            let _ = sink.emit(events.tool_call_progress(progress));
        }
        ProviderStep::ReplayState(replay) => {
            let _ = sink.emit(events.provider_replay_updated(replay));
        }
        _ => {}
    }
    Ok(())
}

impl RuntimeProviderTurnOutput {
    fn response(response: RuntimeModelResponse) -> Self {
        Self::Response(response)
    }

    fn terminal(error: RuntimeTurnStartError) -> Self {
        Self::Terminal(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::{Conversation, Message};
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::tool_execution::policy_for_tool_execution;

    #[derive(Debug)]
    struct FailingProviderStepIngress;

    impl crate::runtime_surface::RuntimeProviderResponseIngress for FailingProviderStepIngress {
        fn commit_response(&self, _response: &RuntimeModelResponse) -> io::Result<()> {
            Ok(())
        }

        fn commit_provider_step(
            &self,
            _identity: &ModelResponseIdentity,
            _step: &ProviderStep,
        ) -> io::Result<()> {
            Err(io::Error::other("semantic stream unavailable"))
        }

        fn commit_tool_results(
            &self,
            _results: &[orca_core::tool_types::ToolResult],
        ) -> io::Result<()> {
            Ok(())
        }
    }

    fn runtime_response(response: ProviderResponse) -> RuntimeModelResponse {
        RuntimeModelResponse::new(response, orca_core::thread_identity::TurnId::new())
    }

    fn response_identity() -> ModelResponseIdentity {
        ModelResponseIdentity::new(orca_core::thread_identity::TurnId::new())
    }

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
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

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("final provider response must not execute child agents")
    }

    #[test]
    fn provider_turn_error_handler_emits_failure_event_for_non_compaction_errors() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
        );
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
        let mut events = EventFactory::new("provider-error-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let subagent_type = SubagentType::General;
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            RuntimeTurnContext::new(cwd, "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();

        let outcome = RuntimeProviderTurnStep::new()
            .handle_provider_error(
                &response,
                &mut compaction,
                &mut conversation,
                &RuntimeCompactionRetryState::default(),
            )
            .expect("provider error handling succeeds");

        match outcome {
            RuntimeProviderErrorOutcome::Failed(error) => {
                assert_eq!(error, "DeepSeek provider error: quota");
            }
            _ => panic!("expected non-compaction provider error to fail"),
        }
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_retry_marks_root_task_and_emits_visible_summary() {
        let registry = TaskRegistry::new("provider-retry-root".to_string());
        let task = registry.create_main_session("retry prompt".to_string());
        registry.mark_running(&task.id).expect("root task running");
        let mut events = EventFactory::new("provider-retry-root".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        record_provider_retry(&registry, Some(&task.id), &mut events, &mut sink)
            .expect("record provider retry");

        let summary = registry.summary(&task.id).expect("root task summary");
        assert_eq!(summary.retry_count, 1);
        assert!(summary.error.is_some());
        assert!(
            String::from_utf8(output)
                .expect("event output")
                .contains("retryCount")
        );
    }

    #[test]
    fn provider_delta_emits_tool_call_progress_event() {
        let mut events = EventFactory::new("provider-progress-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let progress = orca_core::provider_types::ToolCallProgress {
            id: "call_1".to_string(),
            function_name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        };

        emit_provider_delta(
            &ProviderStep::ToolCallProgress(progress),
            &response_identity(),
            None,
            true,
            &mut events,
            &mut sink,
        )
        .expect("legacy-only progress emission");

        drop(sink);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"tool.call.progress\""));
        assert!(output.contains("\"id\":\"call_1\""));
        assert!(output.contains("\"name\":\"write_file\""));
        assert!(output.contains("\"arguments_bytes\":12345"));
    }

    #[test]
    fn provider_delta_requires_typed_commit_before_legacy_visibility() {
        let mut events = EventFactory::new("provider-semantic-before-legacy".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        let error = emit_provider_delta(
            &ProviderStep::MessageDelta("must stay hidden".to_string()),
            &response_identity(),
            Some(&FailingProviderStepIngress),
            true,
            &mut events,
            &mut sink,
        )
        .expect_err("semantic failure must precede legacy delta visibility");

        assert_eq!(error.to_string(), "semantic stream unavailable");
        drop(sink);
        assert!(output.is_empty());
    }

    #[test]
    fn cancelled_provider_turn_preserves_completed_usage() {
        let mut lifecycle =
            crate::lifecycle::RuntimeSessionLifecycle::new("provider-cancelled-usage".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut conversation = Conversation::new();
        conversation.add_user("mock_usage_then_cancel".to_string());
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::FLASH_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let cancel = CancelToken::new();
        let mut cost_tracker = CostTracker::new(Some(orca_core::model::FLASH_MODEL));
        let mut events = EventFactory::new("provider-cancelled-usage".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        let response = RuntimeProviderTurnStep::new()
            .run(RuntimeProviderTurnInput {
                actor: &mut actor,
                provider: ProviderKind::Mock,
                runtime_system_messages: &[],
                provider_config: &provider_config,
                turn_context: RuntimeTurnContext::new(
                    Path::new("."),
                    "mock_usage_then_cancel",
                    0,
                    true,
                    &SubagentType::General,
                ),
                hooks: &hooks,
                cancel: &cancel,
                max_budget_usd: None,
                context_window: 1_000_000,
                io: RuntimeProviderTurnIo {
                    conversation: &mut conversation,
                    cost_tracker: &mut cost_tracker,
                    events: &mut events,
                    sink: &mut sink,
                    history_writer: None,
                },
            })
            .expect("provider turn");

        let RuntimeProviderTurnOutput::Terminal(error) = response else {
            panic!("cancelled provider turn must return a terminal error");
        };
        assert_eq!(error.status, RunStatus::Cancelled);
        assert_eq!(cost_tracker.totals().input_tokens, 120);
        assert_eq!(cost_tracker.totals().output_tokens, 30);
        assert_eq!(cost_tracker.totals().cache_tokens, 10);
        drop(sink);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert_eq!(output.matches("\"type\":\"usage.updated\"").count(), 1);
        // Real-usage-driven context observability accompanies usage: the prompt
        // tokens for this request (120) against the full model window.
        assert_eq!(output.matches("\"type\":\"context.updated\"").count(), 1);
        assert!(output.contains("\"used_tokens\":120"));
        assert!(output.contains("\"limit_tokens\":1000000"));
        assert!(!output.contains("\"type\":\"error\""));
    }

    #[test]
    fn provider_turn_injects_runtime_system_messages_without_mutating_conversation() {
        let mut lifecycle = crate::lifecycle::RuntimeSessionLifecycle::new(
            "provider-runtime-system-directive".to_string(),
        );
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut conversation = Conversation::new();
        conversation.add_system("base system".to_string());
        conversation.add_user("mock_system_echo".to_string());
        let runtime_system_messages = vec!["runtime directive context".to_string()];
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::PRO_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let cancel = CancelToken::new();
        let mut cost_tracker = CostTracker::new(None);
        let mut events = EventFactory::new("provider-runtime-system-directive".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        let result = RuntimeProviderTurnStep::new()
            .run(RuntimeProviderTurnInput {
                actor: &mut actor,
                provider: ProviderKind::Mock,
                runtime_system_messages: runtime_system_messages.as_slice(),
                provider_config: &provider_config,
                turn_context: RuntimeTurnContext::new(
                    Path::new("."),
                    "mock_system_echo",
                    0,
                    true,
                    &orca_core::subagent_types::SubagentType::General,
                ),
                hooks: &hooks,
                cancel: &cancel,
                max_budget_usd: None,
                context_window: 1_000_000,
                io: RuntimeProviderTurnIo {
                    conversation: &mut conversation,
                    cost_tracker: &mut cost_tracker,
                    events: &mut events,
                    sink: &mut sink,
                    history_writer: None,
                },
            })
            .expect("provider turn");

        let RuntimeProviderTurnOutput::Response(response) = result else {
            panic!("provider turn must return a response");
        };
        assert!(
            response
                .response
                .assistant_content
                .as_deref()
                .unwrap_or_default()
                .contains("runtime directive context")
        );
        assert!(conversation.messages.iter().all(|message| {
            !message
                .content_str()
                .unwrap_or_default()
                .contains("runtime directive context")
        }));
    }

    #[test]
    fn provider_error_step_returns_failure_and_resets_reactive_state() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
        );
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
        let mut events = EventFactory::new("provider-error-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let subagent_type = SubagentType::General;
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            RuntimeTurnContext::new(cwd, "", 0, true, &subagent_type),
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();
        let mut step = RuntimeProviderErrorStep::new();
        step.mark_reactive_compacted_for_test();

        let outcome = step
            .handle(&response, &mut compaction, &mut conversation)
            .expect("provider error step succeeds");

        match outcome {
            RuntimeProviderErrorStepOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Failed);
                assert_eq!(error.message, "DeepSeek provider error: quota");
            }
            _ => panic!("expected provider error step failure"),
        }
        assert!(!step.reactive_compacted_for_test());
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_error_result_step_folds_continue_loop_and_failure() {
        let no_error =
            RuntimeProviderErrorResultStep::new().fold(RuntimeProviderErrorStepOutcome::NoError);
        assert!(matches!(no_error, RuntimeProviderErrorResult::ContinueTurn));

        let retry = RuntimeProviderErrorResultStep::new()
            .fold(RuntimeProviderErrorStepOutcome::ContinueAfterCompaction);
        assert!(matches!(retry, RuntimeProviderErrorResult::ContinueLoop));

        let failed = RuntimeProviderErrorResultStep::new().fold(
            RuntimeProviderErrorStepOutcome::Failed(RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::Unclassified,
                message: "provider failed".to_string(),
            }),
        );
        match failed {
            RuntimeProviderErrorResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("provider failed"));
            }
            RuntimeProviderErrorResult::ContinueTurn | RuntimeProviderErrorResult::ContinueLoop => {
                panic!("provider error failure should return loop result")
            }
        }
    }

    #[test]
    fn provider_response_step_records_final_assistant_message() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-response-final".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-response-final".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let outcome = RuntimeProviderResponseStep::new()
            .handle(
                runtime_response(response),
                RuntimeProviderResponseInput {
                    step_context,
                    sampling_state: &mut sampling_state,
                    io: RuntimeProviderResponseIo {
                        events: &mut events,
                        sink: &mut sink,
                        conversation: &mut conversation,
                        history_writer: None,
                        cost_tracker: &mut cost_tracker,
                        background_workflows: &mut background_workflows,
                    },
                },
                RuntimeProviderResponseExecutors {
                    workflow_child_executor: unused_child_executor::<
                        crate::workflow::runner::SharedEventBuffer,
                    >,
                    batch_child_executor: unused_child_executor::<io::Sink>,
                },
            )
            .expect("handle provider response");

        match outcome {
            RuntimeProviderResponseOutcome::Success { final_message } => {
                assert_eq!(final_message.as_deref(), Some("done"));
            }
            _ => panic!("final response should complete the agent loop"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Assistant { content, tool_calls, .. }
                if content.as_deref() == Some("done") && tool_calls.is_empty())
        );
    }

    #[test]
    fn provider_response_step_auto_memory_honors_cancelled_turn() {
        // Synchronous step handling only; the exclusive redirect serializes
        // the env switch against every other test and restores on panic.
        crate::history::with_redirected_orca_home("auto-memory-cancelled", |home| {
            let mut config = config();
            config.auto_memory = true;
            let response = ProviderResponse {
                steps: vec![ProviderStep::MessageDelta("done".to_string())],
                assistant_content: Some("done".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
            let cwd = tempfile::tempdir().expect("cwd");
            let mut events =
                EventFactory::new("provider-response-auto-memory-cancelled".to_string());
            let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
            let mut conversation = Conversation::new();
            conversation.add_user("remember the runtime ownership boundary".to_string());
            let instructions = ProjectInstructions::default();
            let memory = MemoryBlock::default();
            let mcp_registry = McpRegistry::default();
            let hooks = HookRunner::default();
            let mut cost_tracker = CostTracker::new(None);
            let cancel = CancelToken::new();
            cancel.cancel();
            let task_registry =
                TaskRegistry::new("provider-response-auto-memory-cancelled".to_string());
            let mut background_workflows = Vec::new();
            let mut sampling_state = RuntimeSamplingRequestState::new();
            let policy = policy_for_tool_execution(&config);
            let step_context = RuntimeStepContext::new(
                &config,
                cwd.path(),
                AgentToolPolicyContext::unrestricted(),
                0,
                true,
                &policy,
                &instructions,
                &memory,
                &mcp_registry,
                &hooks,
                &cancel,
                &task_registry,
                None,
                None,
                None,
                None,
                None,
            );

            let outcome = RuntimeProviderResponseStep::new()
                .handle(
                    runtime_response(response),
                    RuntimeProviderResponseInput {
                        step_context,
                        sampling_state: &mut sampling_state,
                        io: RuntimeProviderResponseIo {
                            events: &mut events,
                            sink: &mut sink,
                            conversation: &mut conversation,
                            history_writer: None,
                            cost_tracker: &mut cost_tracker,
                            background_workflows: &mut background_workflows,
                        },
                    },
                    RuntimeProviderResponseExecutors {
                        workflow_child_executor: unused_child_executor::<
                            crate::workflow::runner::SharedEventBuffer,
                        >,
                        batch_child_executor: unused_child_executor::<io::Sink>,
                    },
                )
                .expect("handle cancelled auto-memory response");

            assert!(matches!(
                outcome,
                RuntimeProviderResponseOutcome::Success { .. }
            ));
            assert!(!home.join("memory").exists());
            assert!(
                !String::from_utf8(sink.writer_mut().clone())
                    .unwrap()
                    .contains("memory extraction failed")
            );
        });
    }

    #[test]
    fn provider_cycle_step_handles_final_response() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-cycle-final".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-cycle-final".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
            None,
            None,
        );

        let result = RuntimeTurnProviderCycleStep::new()
            .handle_response(
                runtime_response(response),
                RuntimeProviderResponseInput {
                    step_context,
                    sampling_state: &mut sampling_state,
                    io: RuntimeProviderResponseIo {
                        events: &mut events,
                        sink: &mut sink,
                        conversation: &mut conversation,
                        history_writer: None,
                        cost_tracker: &mut cost_tracker,
                        background_workflows: &mut background_workflows,
                    },
                },
                RuntimeProviderResponseExecutors {
                    workflow_child_executor: unused_child_executor::<
                        crate::workflow::runner::SharedEventBuffer,
                    >,
                    batch_child_executor: unused_child_executor::<io::Sink>,
                },
            )
            .expect("handle provider cycle response");

        match result {
            RuntimeTurnProviderCycleResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
                assert_eq!(result.error, None);
            }
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                panic!("final response should return agent-loop result")
            }
            RuntimeTurnProviderCycleResult::Suspended(_) => {
                panic!("response-only test cannot suspend")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Assistant { content, tool_calls, .. }
                if content.as_deref() == Some("done") && tool_calls.is_empty())
        );
    }

    #[test]
    fn provider_cycle_step_can_start_from_continuation_response_without_provider_call() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("continued".to_string())],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
        );
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::PRO_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-cycle-continuation".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        conversation.add_user("continue".to_string());
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-cycle-continuation".to_string());
        let mut background_workflows = Vec::new();
        let policy = policy_for_tool_execution(&config);
        let extension_registry = crate::extension::ExtensionRegistry::default();
        let thread_extensions = std::sync::Arc::new(crate::extension::ExtensionData::new("thread"));
        let turn_extensions = std::sync::Arc::new(crate::extension::ExtensionData::new("turn"));
        let extension_state = crate::lifecycle::RuntimeTurnExtensionState::new(
            extension_registry,
            thread_extensions,
            turn_extensions,
        );
        let mut prepared_conversation =
            crate::runtime_conversation_bootstrap::RuntimeConversationBootstrapStep::new().prepare(
                crate::runtime_conversation_bootstrap::AgentConversationContext::borrowed(
                    &mut conversation,
                    None,
                ),
                cwd.path(),
                "continue",
                0,
                &orca_core::subagent_types::SubagentType::General,
                &instructions,
                config.approval_mode,
                &memory,
            );
        let mut lifecycle = crate::runtime_lifecycle::RuntimeSessionLifecycle::new(
            "provider-cycle-continuation".to_string(),
        );
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);

        let result = RuntimeTurnProviderCycleStep::new()
            .run(
                RuntimeProviderCycleInput {
                    actor: &mut actor,
                    provider: ProviderKind::DeepSeek,
                    continuation: Some(RuntimeTurnContinuation::from_response(
                        response,
                        orca_core::thread_identity::TurnId::new(),
                    )),
                    turn_context: RuntimeTurnContext::new(
                        cwd.path(),
                        "continue",
                        0,
                        true,
                        &orca_core::subagent_types::SubagentType::General,
                    ),
                    turn_provider_config: &provider_config,
                    runtime_system_messages: &[],
                    context_config: &context_config,
                    base_provider_config: &provider_config,
                    capabilities: RuntimeStepCapabilitySnapshot::new(
                        &instructions,
                        &memory,
                        &mcp_registry,
                        &hooks,
                        &cancel,
                        &task_registry,
                        None,
                        None,
                        None,
                        None,
                        None,
                    ),
                    cost_tracker: &mut cost_tracker,
                    max_budget_usd: None,
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut prepared_conversation,
                    config: &config,
                    tool_policy: AgentToolPolicyContext::unrestricted(),
                    policy: &policy,
                    extensions: extension_state.extension_context(),
                    background_workflows: &mut background_workflows,
                },
                unused_child_executor::<crate::workflow::runner::SharedEventBuffer>,
                unused_child_executor::<io::Sink>,
            )
            .expect("run provider cycle from continuation");

        match result {
            RuntimeTurnProviderCycleResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("continued"));
                assert_eq!(result.error, None);
            }
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                panic!("final continuation response should return agent-loop result")
            }
            RuntimeTurnProviderCycleResult::Suspended(_) => {
                panic!("continuation-only test cannot suspend")
            }
        }
        let (conversation, _) = prepared_conversation.parts_mut();
        assert!(
            matches!(conversation.messages.last(), Some(Message::Assistant { content, .. })
                if content.as_deref() == Some("continued"))
        );
    }

    #[test]
    fn provider_turn_output_preserves_terminal_error() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            reason: TurnEndReason::Cancelled,
            message: "turn cancelled".to_string(),
        });

        let error = match output {
            RuntimeProviderTurnOutput::Terminal(error) => error,
            _ => panic!("expected terminal error"),
        };

        assert_eq!(error.status, RunStatus::Cancelled);
        assert_eq!(error.message, "turn cancelled");
    }

    #[test]
    fn provider_turn_result_step_suppresses_cancelled_error_event() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            reason: TurnEndReason::Cancelled,
            message: "turn cancelled".to_string(),
        });
        let mut events = EventFactory::new("provider-turn-result".to_string());
        let mut emitted = Vec::new();
        let mut sink = EventSink::new(&mut emitted, OutputFormat::Jsonl);

        let outcome = RuntimeProviderTurnResultStep::new()
            .fold(output, &mut events, &mut sink, true)
            .expect("fold provider turn result");

        match outcome {
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Cancelled);
                assert_eq!(error.message, "turn cancelled");
            }
            _ => panic!("expected cancelled provider turn to fail"),
        }
        drop(sink);
        assert!(emitted.is_empty());
    }

    #[test]
    fn provider_turn_result_result_step_folds_response_and_failure() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("hello".to_string())],
            assistant_content: Some("hello".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let success = RuntimeProviderTurnResultResultStep::new().fold(
            RuntimeProviderTurnResultOutcome::Response(runtime_response(response)),
        );
        match success {
            RuntimeProviderTurnResultResult::Response(response) => {
                assert_eq!(
                    response.response.assistant_content.as_deref(),
                    Some("hello")
                );
            }
            RuntimeProviderTurnResultResult::Return(_) => {
                panic!("provider response should continue the turn")
            }
            RuntimeProviderTurnResultResult::Suspended(_) => {
                panic!("provider response fixture cannot suspend")
            }
        }

        let failed = RuntimeProviderTurnResultResultStep::new().fold(
            RuntimeProviderTurnResultOutcome::Failed(RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::Unclassified,
                message: "provider failed".to_string(),
            }),
        );
        match failed {
            RuntimeProviderTurnResultResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("provider failed"));
            }
            RuntimeProviderTurnResultResult::Response(_) => {
                panic!("provider failure should return loop result")
            }
            RuntimeProviderTurnResultResult::Suspended(_) => {
                panic!("provider failure fixture cannot suspend")
            }
        }
    }

    #[test]
    fn provider_response_result_step_folds_success_return_and_continue() {
        let success = RuntimeProviderResponseResultStep::new().fold(
            RuntimeProviderResponseOutcome::Success {
                final_message: Some("done".to_string()),
            },
        );
        match success {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
                assert_eq!(result.error, None);
            }
            RuntimeProviderResponseResult::Continue => panic!("success should return loop result"),
        }

        let terminal =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Return {
                status: RunStatus::Cancelled,
                error: Some("cancelled".to_string()),
            });
        match terminal {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Cancelled);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("cancelled"));
            }
            RuntimeProviderResponseResult::Continue => panic!("terminal outcome should return"),
        }

        let continuing =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Continue);
        assert!(matches!(
            continuing,
            RuntimeProviderResponseResult::Continue
        ));
    }
}
