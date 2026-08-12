use std::io;

use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_provider::{ProviderConfig, context};

use crate::compaction::RuntimeCompactionStep;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::lifecycle::{AgentLoopResult, RuntimeTaskActor, RuntimeTurnContext};
use crate::operation_context::OperationContext;
use crate::runtime_model_route::{RuntimeModelRouteInput, RuntimeModelRouteStep};
use crate::runtime_steer::{RuntimeSteerInput, RuntimeSteerStep};
use crate::runtime_turn_start::{
    RuntimeTurnStartInput, RuntimeTurnStartResult, RuntimeTurnStartResultStep, RuntimeTurnStartStep,
};
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeTurnOpeningStep;

pub(crate) struct RuntimeTurnOpeningInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) model: &'a ModelSelection,
    pub(crate) model_override: Option<&'a str>,
    pub(crate) cost_tracker: &'a mut CostTracker,
}

pub(crate) enum RuntimeTurnOpeningResult {
    Continue { provider_config: ProviderConfig },
    Return(AgentLoopResult),
}

impl RuntimeTurnOpeningStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn open<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnOpeningInput<'_, '_, W>,
    ) -> io::Result<RuntimeTurnOpeningResult> {
        let turn_context = input.turn_context.clone();

        // The operation's `BudgetController` admits this turn; on exhaustion
        // the loop stops with a typed terminal before any provider request.
        // The previous turn's tools are already settled (the loop only
        // iterates after a committed tool result), so the stop commits a real
        // `checkpoint.created` to the journal before the terminal is durable,
        // and the surfaced terminal carries that checkpoint (resumable).
        if let Err(stop) = input.operation.admit_turn(turn_context.turn_id.as_str())? {
            // Commit the real resume boundary first (session checkpoint with
            // the durable last-committed message id), then the operation
            // journal's checkpoint + terminal; the surfaced terminal is
            // resumable only because the boundary is already durable.
            let terminal = input.operation.commit_budget_stop_with_boundary(
                turn_context.turn_id.as_str(),
                input.events.run_id(),
                input.history_writer.as_deref_mut(),
                &input.cost_tracker.totals(),
                None,
            )?;
            let result = AgentLoopResult::budget_stop(terminal, stop);
            if let Some(error) = result.error.as_deref()
                && turn_context.emit_deltas
            {
                input.sink.emit(input.events.error(error))?;
            }
            return Ok(RuntimeTurnOpeningResult::Return(result));
        }

        RuntimeCompactionStep::new(
            input.provider,
            input.context_config,
            input.provider_config,
            turn_context.clone(),
            input.hooks,
            input.events,
            input.sink,
            input.history_writer.as_deref_mut(),
        )
        .compact_if_needed(input.conversation)?;

        if turn_context.emit_deltas {
            let pressure = context::context_pressure(
                input.conversation,
                input.context_config,
                input.provider_config,
            );
            // First-turn estimate seed on the full-window scale; the real
            // provider-reported occupancy replaces it after the first response.
            input.sink.emit(
                input
                    .events
                    .context_updated(pressure.wire_tokens, input.context_config.max_tokens),
            )?;
        }

        match RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStep::new().start(
            RuntimeTurnStartInput {
                actor: input.actor,
                events: input.events,
                sink: input.sink,
                turn_context: turn_context.clone(),
            },
        )?) {
            RuntimeTurnStartResult::Return(result) => {
                return Ok(RuntimeTurnOpeningResult::Return(result));
            }
            RuntimeTurnStartResult::Continue => {}
        }

        // Soft-land before a hard budget wall: inject a pinned system reminder
        // when remaining budget crosses configured thresholds. Reminders come
        // from the controller and never mutate usage or success state.
        if let Some(message) = input.operation.controller.take_pending_soft_landing() {
            input.conversation.add_system_pinned(message);
        }
        if let Some(message) = input.actor.take_pending_cost_budget_soft_landing() {
            input.conversation.add_system_pinned(message);
        }

        let turn_provider_config = RuntimeModelRouteStep::new()
            .route(RuntimeModelRouteInput {
                actor: input.actor,
                model: input.model,
                turn_context: turn_context.clone(),
                model_override: input.model_override,
                provider_config: input.provider_config,
                cost_tracker: input.cost_tracker,
                events: input.events,
                sink: input.sink,
            })?
            .provider_config;

        RuntimeSteerStep::new().apply(RuntimeSteerInput {
            turn_context,
            conversation: input.conversation,
            history_writer: input.history_writer.as_deref_mut(),
        })?;

        Ok(RuntimeTurnOpeningResult::Continue {
            provider_config: turn_provider_config,
        })
    }
}
