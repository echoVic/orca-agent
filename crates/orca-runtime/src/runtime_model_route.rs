use std::io;

use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_provider::ProviderConfig;

use crate::cost::CostTracker;
use crate::lifecycle::{RuntimeModelTurn, RuntimeTaskActor, RuntimeTurnContext};

pub(crate) struct RuntimeModelRouteStep;

pub(crate) struct RuntimeModelRouteInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) model: &'a ModelSelection,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) has_images: bool,
    pub(crate) model_override: Option<&'a str>,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
}

impl RuntimeModelRouteStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn route<W: io::Write>(
        &mut self,
        input: RuntimeModelRouteInput<'_, '_, W>,
    ) -> io::Result<RuntimeModelTurn> {
        let RuntimeTurnContext {
            cwd: _,
            prompt: _,
            subagent_depth: _,
            emit_deltas,
            subagent_type,
            continuation: _,
            steer_handle: _,
            ..
        } = input.turn_context;

        let routed_model = input.actor.route_model_turn(
            input.model,
            subagent_type,
            input.model_override,
            input.has_images,
            input.provider_config,
            input.cost_tracker,
        );
        if emit_deltas {
            input
                .sink
                .emit(input.events.model_routed(&routed_model.decision))?;
        }
        Ok(routed_model)
    }
}
