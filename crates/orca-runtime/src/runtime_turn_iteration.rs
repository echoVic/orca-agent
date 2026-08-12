use std::io;

use crate::agent_child::ChildAgentExecutor;
use crate::lifecycle::{
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnDeps, RuntimeTurnLoopIterationState,
};
use crate::operation_context::OperationContext;
use crate::provider_stream::RuntimeProviderSuspension;
use crate::provider_turn::{
    RuntimeProviderCycleInput, RuntimeTurnProviderCycleResult, RuntimeTurnProviderCycleStep,
};
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_loop::{
    RuntimeTurnOutputContext, RuntimeTurnPolicyContext, RuntimeTurnProviderContext,
    RuntimeTurnRequestContext, RuntimeTurnWorkflowContext,
};
use crate::runtime_turn_opening::{
    RuntimeTurnOpeningInput, RuntimeTurnOpeningResult, RuntimeTurnOpeningStep,
};
use crate::step_context::RuntimeStepCapabilitySnapshot;
use crate::workflow::runner::SharedEventBuffer;

pub(crate) struct RuntimeTurnIterationStep {
    opening_step: RuntimeTurnOpeningStep,
    provider_cycle_step: RuntimeTurnProviderCycleStep,
}

pub(crate) struct RuntimeTurnIterationInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) loop_state: RuntimeTurnLoopIterationState<'a>,
    pub(crate) policy: RuntimeTurnPolicyContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) enum RuntimeTurnIterationResult {
    ContinueLoop,
    Return(AgentLoopResult),
    Suspended(RuntimeProviderSuspension),
}

impl RuntimeTurnIterationStep {
    pub(crate) fn new() -> Self {
        Self {
            opening_step: RuntimeTurnOpeningStep::new(),
            provider_cycle_step: RuntimeTurnProviderCycleStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        input: RuntimeTurnIterationInput<'_, '_, W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnIterationResult> {
        let turn_context = input.request.turn_context;
        let continuation = turn_context.continuation.clone();

        let turn_provider_config = {
            let (conversation, history_writer) = input.prepared_conversation.parts_mut();
            match self.opening_step.open(RuntimeTurnOpeningInput {
                actor: input.actor,
                operation: &mut *input.operation,
                provider: input.provider_context.provider,
                context_config: input.provider_context.context_config,
                provider_config: input.provider_context.provider_config,
                turn_context: turn_context.clone(),
                hooks: input.deps.hooks,
                events: input.output.events,
                sink: input.output.sink,
                conversation,
                history_writer,
                model: input.provider_context.model,
                model_override: input.loop_state.model_override,
                cost_tracker: input.loop_state.cost_tracker,
            })? {
                RuntimeTurnOpeningResult::Continue { provider_config } => provider_config,
                RuntimeTurnOpeningResult::Return(result) => {
                    return Ok(RuntimeTurnIterationResult::Return(result));
                }
            }
        };

        match self.provider_cycle_step.run(
            RuntimeProviderCycleInput {
                actor: input.actor,
                operation: &mut *input.operation,
                provider: input.provider_context.provider,
                continuation,
                turn_context,
                turn_provider_config: &turn_provider_config,
                runtime_system_messages: input.loop_state.runtime_system_messages,
                context_config: input.provider_context.context_config,
                base_provider_config: input.provider_context.provider_config,
                capabilities: RuntimeStepCapabilitySnapshot::new(
                    input.deps.instructions,
                    input.deps.memory,
                    input.deps.mcp_registry,
                    input.deps.hooks,
                    input.loop_state.cancel,
                    input.loop_state.task_registry,
                    input.workflow.workflow_ipc,
                    input.deps.turn_interactions.approval_handler(),
                    input.deps.turn_interactions.permission_handler(),
                    input.deps.turn_interactions.user_input_handler(),
                    input.deps.turn_interactions.mcp_elicitation_handler(),
                ),
                cost_tracker: input.loop_state.cost_tracker,
                events: input.output.events,
                sink: input.output.sink,
                conversation: input.prepared_conversation,
                config: input.policy.config,
                tool_policy: input.policy.tool_policy,
                policy: input.policy.approval_policy,
                extensions: input.loop_state.extensions,
                background_workflows: input.workflow.background_workflows,
            },
            workflow_child_executor,
            batch_child_executor,
        )? {
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                Ok(RuntimeTurnIterationResult::ContinueLoop)
            }
            RuntimeTurnProviderCycleResult::Return(result) => {
                Ok(RuntimeTurnIterationResult::Return(result))
            }
            RuntimeTurnProviderCycleResult::Suspended(suspension) => {
                Ok(RuntimeTurnIterationResult::Suspended(suspension))
            }
        }
    }
}
