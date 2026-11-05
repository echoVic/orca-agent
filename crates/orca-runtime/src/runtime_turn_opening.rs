use std::io;

use orca_core::cancel::CancelToken;
use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::{ImageRouteDecision, ModelSelection};
use orca_provider::{ProviderConfig, context};

use crate::compaction::RuntimeCompactionStep;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::image_routing::conversation_has_images;
use crate::lifecycle::{AgentLoopResult, RuntimeTaskActor, RuntimeTurnContext, TurnEndReason};
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
    pub(crate) cancel: &'a CancelToken,
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
    Continue {
        provider_config: ProviderConfig,
        image_route: ImageRouteDecision,
    },
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
        if input.cancel.is_cancelled() {
            return Ok(RuntimeTurnOpeningResult::Return(AgentLoopResult::terminal(
                RunStatus::Cancelled,
                TurnEndReason::Cancelled,
                Some("turn cancelled".to_string()),
                None,
            )));
        }

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
                stop,
            )?;
            let result = AgentLoopResult::budget_stop(terminal, stop);
            if let Some(error) = result.error.as_deref()
                && turn_context.emit_deltas
            {
                input.sink.emit(input.events.error(error))?;
            }
            return Ok(RuntimeTurnOpeningResult::Return(result));
        }

        // Compute once before compaction/hooks; every summary shares this
        // fixed operation deadline instead of renewing a per-request timeout.
        let deadline = input.operation.remaining_deadline()?;
        let prepared = RuntimeCompactionStep::new(
            input.provider,
            input.context_config,
            input.provider_config,
            turn_context.clone(),
            input.hooks,
            input.events,
            input.sink,
            input.history_writer.as_deref_mut(),
        )
        .with_cancel(input.cancel)
        .with_deadline(deadline)
        .prepare_request(input.conversation)?;
        // Local fallback can make the prompt fit after cancellation/expiry.
        // Stop before turn hooks, image routing or the main model request.
        if input.cancel.is_cancelled() {
            return Ok(RuntimeTurnOpeningResult::Return(AgentLoopResult::terminal(
                RunStatus::Cancelled,
                TurnEndReason::Cancelled,
                Some("turn cancelled".to_string()),
                None,
            )));
        }
        if let Err(stop) = input.operation.sync_wall_time()? {
            let terminal = input.operation.commit_budget_stop_with_boundary(
                turn_context.turn_id.as_str(),
                input.events.run_id(),
                input.history_writer.as_deref_mut(),
                &input.cost_tracker.totals(),
                None,
                stop,
            )?;
            return Ok(RuntimeTurnOpeningResult::Return(
                AgentLoopResult::budget_stop(terminal, stop),
            ));
        }
        if let Err(overflow) = prepared {
            let message = crate::compaction::context_overflow_message(overflow.prompt_tokens);
            if turn_context.emit_deltas {
                input.sink.emit(input.events.error(&message))?;
            }
            return Ok(RuntimeTurnOpeningResult::Return(AgentLoopResult::failure(
                RunStatus::Failed,
                message,
            )));
        }

        if turn_context.emit_deltas {
            let pressure = context::context_pressure(
                input.conversation,
                input.context_config,
                input.provider_config,
            );
            // First-turn estimate seed against the compaction line; the real
            // provider-reported occupancy replaces it after the first response.
            input.sink.emit(
                input
                    .events
                    .context_updated(pressure.wire_tokens, input.context_config.soft_limit()),
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

        // Soft-land before a hard budget wall: inject a system reminder when
        // remaining budget crosses configured thresholds. Reminders come from
        // the controller and never mutate usage or success state.
        if let Some(message) = input.operation.controller.take_pending_soft_landing() {
            input.conversation.add_system(message);
        }
        if let Some(message) = input.actor.take_pending_cost_budget_soft_landing() {
            input.conversation.add_system(message);
        }

        let routed_model = RuntimeModelRouteStep::new().route(RuntimeModelRouteInput {
            actor: input.actor,
            model: input.model,
            turn_context: turn_context.clone(),
            has_images: conversation_has_images(input.conversation),
            model_override: input.model_override,
            provider_config: input.provider_config,
            cost_tracker: input.cost_tracker,
            events: input.events,
            sink: input.sink,
        })?;
        let image_route = routed_model.decision.image_route;
        let turn_provider_config = routed_model.provider_config;

        RuntimeSteerStep::new().apply(RuntimeSteerInput {
            turn_context,
            conversation: input.conversation,
            history_writer: input.history_writer.as_deref_mut(),
        })?;

        Ok(RuntimeTurnOpeningResult::Continue {
            provider_config: turn_provider_config,
            image_route,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::OutputFormat;
    use orca_core::subagent_types::SubagentType;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn opening_summary_case(
        wall_time_ms: Option<u64>,
        cancel_on_accept: bool,
    ) -> (RuntimeTurnOpeningResult, Duration) {
        let cancel = CancelToken::new();
        let server_cancel = cancel.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(3) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept summary: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 16 * 1024];
            let _ = stream.read(&mut request);
            if cancel_on_accept {
                server_cancel.cancel();
            }
            // The endpoint waits beyond the entire operation budget.
            std::thread::sleep(Duration::from_millis(1_200));
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"retained facts\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());
        conversation.add_user("archived evidence ".repeat(650));
        conversation.add_user("current request".to_string());
        let context_config = context::ContextConfig {
            max_tokens: 50_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(50_000),
            soft_compact_token_limit: Some(300),
        };
        let provider_config = ProviderConfig {
            api_key: Some("local-fixture-key".into()),
            base_url: Some(format!("http://{address}")),
            model: None,
            reasoning_effort: Default::default(),
            tools_override: Some(vec![]),
            mcp_registry: None,
            external_tools: vec![],
            max_output_tokens: None,
        };
        let mut lifecycle = crate::lifecycle::RuntimeSessionLifecycle::new("opening-deadline");
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut operation = OperationContext::for_tests(
            orca_core::budget::BudgetSpec {
                max_wall_time_ms: wall_time_ms,
                ..Default::default()
            },
            "opening-summary-deadline",
        );
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("opening-deadline".into());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let model = ModelSelection::parse(None).unwrap();
        let mut cost = CostTracker::new(None);
        let subagent = SubagentType::General;
        let started = Instant::now();
        let result = RuntimeTurnOpeningStep::new()
            .open(RuntimeTurnOpeningInput {
                actor: &mut actor,
                operation: &mut operation,
                cancel: &cancel,
                provider: ProviderKind::DeepSeek,
                context_config: &context_config,
                provider_config: &provider_config,
                turn_context: RuntimeTurnContext::new(
                    std::path::Path::new("."),
                    "",
                    0,
                    false,
                    &subagent,
                ),
                hooks: &hooks,
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                model: &model,
                model_override: None,
                cost_tracker: &mut cost,
            })
            .unwrap();
        let elapsed = started.elapsed();
        server.join().unwrap();
        (result, elapsed)
    }

    #[test]
    fn automatic_opening_summary_obeys_remaining_operation_wall_time() {
        let (result, elapsed) = opening_summary_case(Some(500), false);
        assert!(
            elapsed < Duration::from_millis(900),
            "automatic summary exceeded remaining operation wall time: {elapsed:?}"
        );
        assert!(
            matches!(
                result,
                RuntimeTurnOpeningResult::Return(AgentLoopResult {
                    terminal: orca_core::budget::OperationTerminal::Stopped {
                        reason: orca_core::budget::StopReason::WallTimeBudget { .. },
                        ..
                    },
                    ..
                })
            ),
            "expired summary must stop before the main model, even after local fallback"
        );
    }

    #[test]
    fn automatic_opening_summary_cancellation_stops_before_main_model() {
        let (result, elapsed) = opening_summary_case(None, true);
        assert!(
            elapsed < Duration::from_millis(900),
            "automatic summary ignored cancellation: {elapsed:?}"
        );
        assert!(
            matches!(
                result,
                RuntimeTurnOpeningResult::Return(AgentLoopResult {
                    status: RunStatus::Cancelled,
                    ..
                })
            ),
            "cancelled summary must stop before the main model, even after local fallback"
        );
    }
}
