use std::io;
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;

use crate::cost::CostTracker;
use crate::extension::{
    ExtensionData, ExtensionRegistry, RuntimeExtensionContext, RuntimeExtensionStores,
};
use crate::lifecycle::{RuntimeTurnExtensionState, RuntimeTurnLoopRuntime, RuntimeTurnLoopState};
use crate::operation_context::OperationContext;
use crate::provider_turn::{RuntimeProviderResponseInput, RuntimeProviderResponseIo};
use crate::runtime_directive::RuntimeDirectiveState;
use crate::runtime_state::RuntimeTurnReducer;
use crate::step_context::{RuntimeSamplingRequestState, RuntimeStepContext};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnKernel<'a> {
    extension_stores: RuntimeExtensionStores<'a>,
    sampling_state: RuntimeSamplingRequestState,
    reducer: RuntimeTurnReducer,
}

impl<'a> RuntimeTurnKernel<'a> {
    pub(crate) fn new(thread_store: &'a ExtensionData, turn_store: &'a ExtensionData) -> Self {
        let extension_stores = RuntimeExtensionStores::new(thread_store, turn_store);
        Self {
            extension_stores,
            sampling_state: RuntimeSamplingRequestState::new(),
            reducer: RuntimeTurnReducer::from_extension_stores(extension_stores),
        }
    }

    pub(crate) fn from_extension_stores(extension_stores: RuntimeExtensionStores<'a>) -> Self {
        Self::new(
            extension_stores.thread_store(),
            extension_stores.turn_store(),
        )
    }

    pub(crate) fn set_preapproved_tool_call_id(&mut self, id: Option<String>) {
        self.sampling_state
            .permission_overlay_mut()
            .set_preapproved_tool_call_id(id);
    }

    pub(crate) fn set_unsandboxed_shell(&mut self, enabled: bool) {
        if enabled {
            self.sampling_state
                .permission_overlay_mut()
                .merge_permissions(&crate::protocol::RequestPermissionProfile {
                    shell: Some(crate::protocol::RequestShellPermissions { unsandboxed: true }),
                    ..Default::default()
                });
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn bind_step_context(
        &self,
        mut step_context: RuntimeStepContext<'a>,
        extension_registry: &'a ExtensionRegistry,
    ) -> RuntimeStepContext<'a> {
        step_context.extensions = Some(RuntimeExtensionContext::new(
            extension_registry,
            self.extension_stores,
        ));
        step_context
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn provider_response_input<'response, W: io::Write>(
        &'response mut self,
        mut step_context: RuntimeStepContext<'response>,
        extension_registry: &'response ExtensionRegistry,
        operation: &'response mut OperationContext,
        events: &'response mut EventFactory,
        sink: &'response mut EventSink<W>,
        conversation: &'response mut Conversation,
        history_writer: Option<&'response mut SessionWriter>,
        cost_tracker: &'response mut CostTracker,
        background_workflows: &'response mut Vec<BackgroundWorkflowRun>,
        checkpoint_observer: Option<
            &'response dyn crate::child_agent_types::ChildAgentCheckpointSink,
        >,
    ) -> RuntimeProviderResponseInput<'response, W>
    where
        'a: 'response,
    {
        step_context.extensions = Some(RuntimeExtensionContext::new(
            extension_registry,
            self.extension_stores,
        ));
        RuntimeProviderResponseInput {
            step_context,
            sampling_state: &mut self.sampling_state,
            operation,
            io: RuntimeProviderResponseIo {
                events,
                sink,
                conversation,
                history_writer,
                cost_tracker,
                background_workflows,
                checkpoint_observer,
            },
        }
    }

    pub(crate) fn turn_loop_state<'loop_state>(
        &self,
        directive_state: RuntimeDirectiveState,
        cost_tracker: &'loop_state mut CostTracker,
        cancel: &'loop_state CancelToken,
        task_registry: &'loop_state TaskRegistry,
        extension_registry: ExtensionRegistry,
        thread_extensions: Arc<ExtensionData>,
        turn_extensions: Arc<ExtensionData>,
    ) -> RuntimeTurnLoopState<'loop_state> {
        RuntimeTurnLoopState {
            directive_state,
            runtime: RuntimeTurnLoopRuntime {
                cost_tracker,
                cancel,
                task_registry,
                extensions: RuntimeTurnExtensionState::new(
                    extension_registry,
                    thread_extensions,
                    turn_extensions,
                ),
            },
        }
    }

    #[allow(dead_code)]
    pub(crate) fn reducer(&self) -> RuntimeTurnReducer {
        self.reducer
    }
}
