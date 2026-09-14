use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::agent_continuation::{conversation_has_open_tool_calls, try_last_settled_tool_boundary};
use crate::child_agent_types::ChildAgentCheckpointObservation;
use crate::lifecycle::{
    AgentLoopOutcome, RuntimeTaskActor, RuntimeTurnContext, RuntimeTurnDeps, RuntimeTurnLoopState,
};
use crate::operation_context::OperationContext;
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_iteration::{
    RuntimeTurnIterationInput, RuntimeTurnIterationResult, RuntimeTurnIterationStep,
};
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnLoopStep {
    iteration_step: RuntimeTurnIterationStep,
}

pub(crate) struct RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'ipc WorkflowIpcContext>,
}

pub(crate) struct RuntimeTurnOutputContext<'events, 'sink, W: io::Write> {
    pub(crate) events: &'events mut EventFactory,
    pub(crate) sink: &'sink mut EventSink<W>,
}

pub(crate) struct RuntimeTurnProviderContext<'a> {
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) model: &'a ModelSelection,
}

pub(crate) struct RuntimeTurnRequestContext<'a> {
    pub(crate) turn_context: RuntimeTurnContext<'a>,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeTurnPolicyContext<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) approval_policy: &'a ApprovalPolicy,
}

pub(crate) struct RuntimeAgentTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) policy: RuntimeTurnPolicyContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) struct RuntimeTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) operation: &'a mut OperationContext,
    pub(crate) provider_context: RuntimeTurnProviderContext<'a>,
    pub(crate) request: RuntimeTurnRequestContext<'a>,
    pub(crate) deps: RuntimeTurnDeps<'a>,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) policy: RuntimeTurnPolicyContext<'a>,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
}

pub(crate) struct RuntimeTurnLoopExecutors {
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput<'a, 'runtime, W> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: &'a mut RuntimeTaskActor<'runtime>,
        operation: &'a mut OperationContext,
        provider_context: RuntimeTurnProviderContext<'a>,
        request: RuntimeTurnRequestContext<'a>,
        deps: RuntimeTurnDeps<'a>,
        output: RuntimeTurnOutputContext<'a, 'a, W>,
        prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
        loop_state: RuntimeTurnLoopState<'a>,
        policy: RuntimeTurnPolicyContext<'a>,
        workflow: RuntimeTurnWorkflowContext<'a, 'a>,
    ) -> Self {
        Self {
            actor,
            operation,
            provider_context,
            request,
            deps,
            output,
            prepared_conversation,
            loop_state,
            policy,
            workflow,
        }
    }

    pub(crate) fn iteration_input<'iter>(
        &'iter mut self,
        provider_config_override: Option<&'iter ProviderConfig>,
    ) -> RuntimeTurnIterationInput<'iter, 'runtime, W> {
        let loop_state = self.loop_state.iteration_state(self.policy.tool_policy);
        let policy = RuntimeTurnPolicyContext::new(
            self.policy.config,
            loop_state.tool_policy,
            self.policy.approval_policy,
        );
        RuntimeTurnIterationInput {
            actor: &mut *self.actor,
            operation: &mut *self.operation,
            provider_context: RuntimeTurnProviderContext::new(
                self.provider_context.provider,
                self.provider_context.context_config,
                provider_config_override.unwrap_or(self.provider_context.provider_config),
                self.provider_context.model,
            ),
            request: self.request.for_iteration(),
            deps: self.deps,
            output: RuntimeTurnOutputContext::new(&mut *self.output.events, &mut *self.output.sink),
            prepared_conversation: &mut *self.prepared_conversation,
            loop_state,
            policy,
            workflow: RuntimeTurnWorkflowContext::new(
                &mut *self.workflow.background_workflows,
                self.workflow.workflow_ipc,
            ),
        }
    }
}

impl<'background, 'ipc> RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) fn new(
        background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'ipc WorkflowIpcContext>,
    ) -> Self {
        Self {
            background_workflows,
            workflow_ipc,
        }
    }
}

impl<'events, 'sink, W: io::Write> RuntimeTurnOutputContext<'events, 'sink, W> {
    pub(crate) fn new(events: &'events mut EventFactory, sink: &'sink mut EventSink<W>) -> Self {
        Self { events, sink }
    }
}

impl<'a> RuntimeTurnProviderContext<'a> {
    pub(crate) fn new(
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        model: &'a ModelSelection,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            model,
        }
    }
}

impl<'a> RuntimeTurnRequestContext<'a> {
    pub(crate) fn new(turn_context: RuntimeTurnContext<'a>) -> Self {
        Self { turn_context }
    }

    fn for_iteration(&mut self) -> Self {
        let turn_context = RuntimeTurnContext {
            turn_id: self.turn_context.turn_id.clone(),
            cwd: self.turn_context.cwd,
            prompt: self.turn_context.prompt,
            subagent_depth: self.turn_context.subagent_depth,
            task_id: self.turn_context.task_id.clone(),
            emit_deltas: self.turn_context.emit_deltas,
            subagent_type: self.turn_context.subagent_type,
            root_task_id: self.turn_context.root_task_id,
            continuation: self.turn_context.continuation.take(),
            steer_handle: self.turn_context.steer_handle,
            execution_policy: self.turn_context.execution_policy,
            provider_suspension_control: self.turn_context.provider_suspension_control,
            provider_response_ingress: self.turn_context.provider_response_ingress,
            workflow_lifecycle_ingress: self.turn_context.workflow_lifecycle_ingress,
            wait_for_background_workflows: self.turn_context.wait_for_background_workflows,
            defer_cancel_terminal: self.turn_context.defer_cancel_terminal,
            permission_handler_owned: self.turn_context.permission_handler_owned.clone(),
        };
        Self { turn_context }
    }
}

impl<'a> RuntimeTurnPolicyContext<'a> {
    pub(crate) fn new(
        config: &'a RunConfig,
        tool_policy: AgentToolPolicyContext<'a>,
        approval_policy: &'a ApprovalPolicy,
    ) -> Self {
        Self {
            config,
            tool_policy,
            approval_policy,
        }
    }
}

impl RuntimeTurnLoopExecutors {
    pub(crate) fn new(
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> Self {
        Self {
            workflow_child_executor,
            batch_child_executor,
        }
    }
}

pub(crate) fn run_agent_turn_loop<W: io::Write>(
    step: &mut RuntimeTurnLoopStep,
    input: RuntimeAgentTurnLoopInput<'_, '_, W>,
    executors: RuntimeTurnLoopExecutors,
) -> io::Result<AgentLoopOutcome> {
    step.run(input.into_turn_loop_input(), executors)
}

impl RuntimeTurnLoopStep {
    pub(crate) fn new() -> Self {
        Self {
            iteration_step: RuntimeTurnIterationStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnLoopInput<'_, '_, W>,
        executors: RuntimeTurnLoopExecutors,
    ) -> io::Result<AgentLoopOutcome> {
        let owner = input
            .request
            .turn_context
            .task_id
            .as_deref()
            .or(input.request.turn_context.root_task_id)
            .map(str::to_string);
        let activity_ingress = input
            .request
            .turn_context
            .workflow_lifecycle_ingress
            .and_then(|ingress| ingress.subagent_activity_ingress());
        let agent_controller = input
            .loop_state
            .runtime
            .extensions
            .extension_context()
            .stores()
            .thread_store()
            .get::<crate::agent_controller::AgentController>();
        crate::runtime_subagent_call::recover_queued_subagent_launches(
            input.policy.config,
            input.deps.mcp_registry,
            input.deps.hooks,
            executors.batch_child_executor,
            activity_ingress,
            input.request.turn_context.permission_handler_owned.clone(),
            input.loop_state.runtime.task_registry,
            owner.as_deref(),
            agent_controller,
        )?;
        let mut summary_provider_config = None;
        loop {
            park_pending_task_wait(&mut input)?;
            deliver_child_results(&mut input)?;
            if let Some(task_id) = input.request.turn_context.task_id.as_deref() {
                let registry = input.loop_state.runtime.task_registry;
                let messages = registry.pending_child_messages(task_id);
                if !conversation_has_open_tool_calls(
                    input.prepared_conversation.checkpoint_parts().0,
                ) {
                    for message in messages {
                        let (conversation, mut writer) = input.prepared_conversation.parts_mut();
                        let content = format!(
                            "[Parent guidance id={}] {}\nThis is guidance from the delegating agent, \
                             not a user instruction or a new task. Preserve the original brief and permissions.",
                            message.message_id, message.text
                        );
                        let present = conversation.messages.iter().any(|existing|
                            matches!(existing, orca_core::conversation::Message::System { content: text, .. }
                                if text == &content));
                        let _appended = match writer.as_deref_mut() {
                            Some(writer) => writer.append_system_delivery_once(&content)?,
                            None => !present,
                        };
                        if !present {
                            conversation.add_system_pinned(content);
                        }
                        emit_safe_child_checkpoint(
                            input.prepared_conversation,
                            input.operation.controller.usage(),
                        )?;
                        registry
                            .ack_child_message(task_id, &message.message_id)
                            .map_err(io::Error::other)?;
                    }
                }
            }
            let subagent_type = input.request.turn_context.subagent_type;
            let conversation = input.prepared_conversation.checkpoint_parts().0;
            let delegated_parent_due =
                crate::investigation_convergence::explicit_read_only_request(conversation)
                    && direct_subagent_children_settled(
                        input.loop_state.runtime.task_registry,
                        owner.as_deref(),
                    )?;
            let summary_due =
                crate::investigation_convergence::summary_prompt_present(conversation)
                    || (crate::investigation_convergence::applies(subagent_type)
                        && crate::investigation_convergence::summary_due(
                            input.policy.config,
                            subagent_type,
                            input.operation.controller.usage(),
                        ))
                    || delegated_parent_due;
            if summary_due {
                let (conversation, _) = input.prepared_conversation.parts_mut();
                if crate::investigation_convergence::ensure_summary_prompt(conversation) {
                    emit_safe_child_checkpoint(
                        input.prepared_conversation,
                        input.operation.controller.usage(),
                    )?;
                }
                if summary_provider_config.is_none() {
                    let mut provider_config = input.provider_context.provider_config.clone();
                    crate::investigation_convergence::disable_tools(&mut provider_config);
                    summary_provider_config = Some(provider_config);
                }
            }
            match self.iteration_step.run(
                input.iteration_input(summary_provider_config.as_ref()),
                executors.workflow_child_executor,
                executors.batch_child_executor,
            )? {
                RuntimeTurnIterationResult::ContinueLoop => {
                    emit_safe_child_checkpoint(
                        input.prepared_conversation,
                        input.operation.controller.usage(),
                    )?;
                    continue;
                }
                RuntimeTurnIterationResult::Return(result) => {
                    let root_or_main =
                        input
                            .request
                            .turn_context
                            .task_id
                            .as_deref()
                            .is_none_or(|id| {
                                input
                                    .loop_state
                                    .runtime
                                    .task_registry
                                    .get(id)
                                    .is_some_and(|task| {
                                        task.task_type
                                            == orca_core::task_types::TaskType::MainSession
                                    })
                            });
                    if root_or_main
                        && input.request.turn_context.wait_for_background_workflows
                        && !input.workflow.background_workflows.is_empty()
                    {
                        crate::workflow_execution::observe_background_workflows(
                            true,
                            input.output.events,
                            input.output.sink,
                            input.workflow.background_workflows,
                            input.loop_state.runtime.task_registry,
                            input.loop_state.runtime.cancel,
                            input.request.turn_context.workflow_lifecycle_ingress,
                        )?;
                        input.operation.refresh_child_budgets()?;
                    }
                    // A natural model ending does not abandon accepted children or
                    // close their parent fence before queued launches can start.
                    if matches!(
                        result.terminal,
                        orca_core::budget::OperationTerminal::Completed { .. }
                    ) && settle_children_before_return(&mut input)?
                    {
                        continue;
                    }
                    if result.status != orca_core::event_schema::RunStatus::ApprovalRequired {
                        let (conversation, observer) =
                            input.prepared_conversation.checkpoint_parts();
                        if observer.is_some() && !conversation_has_open_tool_calls(conversation) {
                            emit_safe_child_checkpoint(
                                input.prepared_conversation,
                                input.operation.controller.usage(),
                            )?;
                        }
                    }
                    return Ok(AgentLoopOutcome::Completed(result));
                }
                RuntimeTurnIterationResult::Suspended(suspension) => {
                    return Ok(AgentLoopOutcome::ProviderSuspended(suspension));
                }
            }
        }
    }
}

fn direct_subagent_children_settled(
    registry: &crate::tasks::TaskRegistry,
    owner: Option<&str>,
) -> io::Result<bool> {
    let Some(owner) = owner else {
        return Ok(false);
    };
    let records = registry.records_snapshot().map_err(io::Error::other)?;
    let mut children = records.iter().filter(|task| {
        task.task_type == orca_core::task_types::TaskType::Subagent
            && task.parent_task_id.as_deref() == Some(owner)
    });
    let Some(first) = children.next() else {
        return Ok(false);
    };
    Ok(!first.status.is_active() && children.all(|task| !task.status.is_active()))
}

/// Drain at every settled model boundary, not only when a new user turn starts.
fn deliver_child_results<W: io::Write>(
    input: &mut RuntimeTurnLoopInput<'_, '_, W>,
) -> io::Result<bool> {
    if conversation_has_open_tool_calls(input.prepared_conversation.checkpoint_parts().0) {
        return Ok(false);
    }
    let registry = input.loop_state.runtime.task_registry;
    let parent = input
        .request
        .turn_context
        .task_id
        .as_deref()
        .or(input.request.turn_context.root_task_id);
    let results = registry.claim_subagent_results_for_parent(parent);
    let delivered = !results.is_empty();
    for result in results {
        let ack = crate::tasks::DeliveryAck::from(&result);
        let content = result.model_notification();
        let (conversation, mut writer) = input.prepared_conversation.parts_mut();
        let present = conversation.messages.iter().any(|message|
            matches!(message, orca_core::conversation::Message::System { content: text, .. } if text == &content));
        if let Some(writer) = writer.as_deref_mut() {
            if let Err(error) = writer.append_system_delivery_once(&content) {
                registry.release_subagent_result_claim(&ack);
                return Err(error);
            }
        }
        if !present {
            conversation.add_system_pinned(content);
        }
        if let Err(error) = emit_safe_child_checkpoint(
            input.prepared_conversation,
            input.operation.controller.usage(),
        ) {
            registry.release_subagent_result_claim(&ack);
            return Err(error);
        }
        registry.ack_subagent_result(&ack);
    }
    Ok(delivered)
}

fn settle_children_before_return<W: io::Write>(
    input: &mut RuntimeTurnLoopInput<'_, '_, W>,
) -> io::Result<bool> {
    let registry = input.loop_state.runtime.task_registry;
    let owner = input
        .request
        .turn_context
        .task_id
        .as_deref()
        .or(input.request.turn_context.root_task_id);
    let live_children = || -> io::Result<Vec<String>> {
        let records = registry.records_snapshot().map_err(io::Error::other)?;
        if let Some(task) = records.iter().find(|task| {
            task.task_type == orca_core::task_types::TaskType::Subagent
                && task.parent_task_id.as_deref() == owner
                && task.continuation_indeterminate
        }) {
            return Err(io::Error::other(format!(
                "child {} has indeterminate execution; its reservation is retained and queued siblings cannot be assumed runnable",
                task.id
            )));
        }
        Ok(records
            .iter()
            .filter(|task| {
                task.task_type == orca_core::task_types::TaskType::Subagent
                    && task.status.is_active()
                    && task.parent_task_id.as_deref() == owner
            })
            .map(|task| task.id.clone())
            .collect())
    };
    let children = live_children()?;
    let waited = !children.is_empty();
    if waited {
        if let Some(id) = input.request.turn_context.task_id.as_deref() {
            registry
                .set_pending_wait(
                    id,
                    Some(crate::tasks::PendingTaskWait {
                        tool_call_id: format!("finish-children:{id}"),
                        task_ids: children,
                        deadline_at_ms: i64::MAX,
                        any: false,
                        state_change: false,
                        baseline: Vec::new(),
                        result: None,
                    }),
                )
                .map_err(io::Error::other)?;
            park_pending_task_wait(input)?;
        } else {
            // The root holds no child execution lease. Keeping its operation
            // alive also keeps the durable parent fence valid for queued work.
            let scope = registry.execution_scope(&input.policy.config.subagents.limits);
            while !live_children()?.is_empty() {
                if input.loop_state.runtime.cancel.is_cancelled()
                    || input.operation.sync_wall_time()?.is_err()
                {
                    break;
                }
                scope.stop_expired(chrono::Utc::now().timestamp_millis());
                scope.wait_for_capacity(
                    Some(input.loop_state.runtime.cancel),
                    std::time::Duration::from_millis(50),
                );
            }
        }
    }
    input.operation.refresh_child_budgets()?;
    Ok(deliver_child_results(input)? || waited)
}

/// A wait owns no execution slot after its tool boundary is durably settled.
fn park_pending_task_wait<W: io::Write>(
    input: &mut RuntimeTurnLoopInput<'_, '_, W>,
) -> io::Result<()> {
    let Some(task_id) = input.request.turn_context.task_id.as_deref() else {
        return Ok(());
    };
    let registry = input.loop_state.runtime.task_registry;
    let Some(mut wait) = registry.get(task_id).and_then(|record| record.pending_wait) else {
        return Ok(());
    };
    if conversation_has_open_tool_calls(input.prepared_conversation.checkpoint_parts().0) {
        return Ok(());
    }
    emit_safe_child_checkpoint(
        input.prepared_conversation,
        input.operation.controller.usage(),
    )?;
    registry.requeue_for_resume(task_id, crate::task_view::WaitReason::Dependency);
    let scope = registry.execution_scope(&input.policy.config.subagents.limits);
    if wait.result.is_none() {
        let (observations, reason) = loop {
            let observations: Vec<_> = wait
                .task_ids
                .iter()
                .map(|id| crate::task_view::TaskView::lookup(registry, id))
                .collect();
            let terminals: Vec<_> = observations
                .iter()
                .map(|view| view.as_ref().is_some_and(|view| view.is_terminal()))
                .collect();
            let completed = if wait.any {
                terminals.iter().any(|value| *value)
            } else {
                terminals.iter().all(|value| *value)
            };
            let changed = wait.state_change
                && wait
                    .task_ids
                    .iter()
                    .zip(&wait.baseline)
                    .any(|(id, before)| {
                        registry
                            .get(id)
                            .is_none_or(|record| record.publication_revision != *before)
                    });
            let now = chrono::Utc::now().timestamp_millis();
            if registry.deadline_expired(task_id) {
                let _ = registry.request_stop_tree(task_id);
            }
            let reason = if input.loop_state.runtime.cancel.is_cancelled()
                || registry.is_cancelled(task_id)
            {
                Some("wait_cancelled")
            } else if observations.iter().any(Option::is_none) {
                Some("target_unavailable")
            } else if completed {
                Some("target_reached_terminal")
            } else if changed {
                Some("state_changed")
            } else if now >= wait.deadline_at_ms {
                Some("wait_elapsed")
            } else {
                None
            };
            if let Some(reason) = reason {
                break (observations, reason);
            }
            scope.wait_for_capacity(
                Some(input.loop_state.runtime.cancel),
                std::time::Duration::from_millis(50),
            );
        };
        wait.result = Some(format!(
            "[Task wait result id={}] {}",
            wait.tool_call_id,
            serde_json::json!({"return_reason": reason, "tasks": observations.into_iter()
            .map(|view| view.map(|view| view.to_json())).collect::<Vec<_>>() })
        ));
        registry
            .set_pending_wait(task_id, Some(wait.clone()))
            .map_err(io::Error::other)?;
    }
    // Persist the exact observation before delivery, so replay cannot change
    // timing fields and inject a second copy after a crash before acknowledgement.
    registry.requeue_for_resume(task_id, crate::task_view::WaitReason::ExecutionCapacity);
    while !input.loop_state.runtime.cancel.is_cancelled() && !registry.is_cancelled(task_id) {
        if scope.acquire(task_id, chrono::Utc::now().timestamp_millis())
            == crate::execution_scope::Admission::Started
        {
            break;
        }
        scope.wait_for_capacity(
            Some(input.loop_state.runtime.cancel),
            std::time::Duration::from_millis(50),
        );
    }
    let content = wait.result.expect("settled wait has a durable observation");
    let (conversation, mut writer) = input.prepared_conversation.parts_mut();
    let present = conversation.messages.iter().any(|message|
        matches!(message, orca_core::conversation::Message::System { content: text, .. } if text == &content));
    let _appended = match writer.as_deref_mut() {
        Some(writer) => writer.append_system_delivery_once(&content)?,
        None => !present,
    };
    if !present {
        conversation.add_system_pinned(content);
    }
    emit_safe_child_checkpoint(
        input.prepared_conversation,
        input.operation.controller.usage(),
    )?;
    registry
        .set_pending_wait(task_id, None)
        .map_err(io::Error::other)?;
    Ok(())
}

/// Emits a checkpoint only after one production child iteration has fully
/// settled. It reports this operation's current cumulative usage and derived
/// trustworthy tool boundary, performs no action without an observer, and
/// propagates validation or persistence failures as explicit I/O errors.
fn emit_safe_child_checkpoint(
    prepared_conversation: &RuntimePreparedConversation<'_>,
    usage: orca_core::budget::BudgetUsage,
) -> io::Result<()> {
    let (conversation, observer) = prepared_conversation.checkpoint_parts();
    let Some(observer) = observer else {
        return Ok(());
    };
    let last_tool_boundary =
        try_last_settled_tool_boundary(conversation).map_err(child_checkpoint_error)?;
    observer
        .checkpoint(ChildAgentCheckpointObservation {
            conversation,
            turn: usage.turns,
            usage,
            last_tool_boundary,
        })
        .map_err(child_checkpoint_error)
}

fn child_checkpoint_error(error: crate::agent_continuation::AgentContinuationError) -> io::Error {
    io::Error::other(format!(
        "child checkpoint failed [{}]: {error}",
        error.contract_code()
    ))
}

impl<'a, 'runtime, W: io::Write> RuntimeAgentTurnLoopInput<'a, 'runtime, W> {
    fn into_turn_loop_input(self) -> RuntimeTurnLoopInput<'a, 'runtime, W> {
        RuntimeTurnLoopInput::new(
            self.actor,
            self.operation,
            self.provider_context,
            self.request,
            self.deps,
            self.output,
            self.prepared_conversation,
            self.loop_state,
            self.policy,
            self.workflow,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Conversation;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::subagent_types::SubagentType;
    use orca_mcp::McpRegistry;

    use crate::cost::CostTracker;
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTurnDeps, RuntimeTurnState};
    use crate::memory::MemoryBlock;
    use crate::runtime_conversation_bootstrap::{
        AgentConversationContext, RuntimeConversationBootstrapStep,
    };
    use crate::tasks::TaskRegistry;
    use crate::tool_execution::policy_for_tool_execution;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            execution_profile: orca_core::capability::ExecutionProfile::Workspace,
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
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: orca_core::config::BudgetConfig::default(),
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

    fn exercise_pending_wait(task_registry: &TaskRegistry, task_id: &str) {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &config.model_runtime,
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
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = policy_for_tool_execution(&config);
        let subagent_type = SubagentType::General;
        let cancel = orca_core::cancel::CancelToken::new();
        let mut cost_tracker = CostTracker::new(None);
        let loop_state =
            RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry).into_loop_state();
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-loop-continuation");
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("turn-loop-continuation".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let mut prepared_conversation = RuntimeConversationBootstrapStep::new().prepare(
            AgentConversationContext::borrowed(&mut conversation, None),
            cwd.path(),
            "continue",
            0,
            &subagent_type,
            &instructions,
            config.approval_mode,
            &memory,
            orca_core::subagent_config::DelegationPolicy::default(),
        );
        let mut background_workflows = Vec::new();
        let mut input = RuntimeTurnLoopInput {
            actor: &mut actor,
            operation: &mut crate::operation_context::OperationContext::for_tests(
                orca_core::budget::BudgetSpec::default(),
                "turn-loop-input-test",
            ),
            provider_context: RuntimeTurnProviderContext::new(
                ProviderKind::DeepSeek,
                &context_config,
                &provider_config,
                &config.model,
            ),
            request: RuntimeTurnRequestContext::new(
                RuntimeTurnContext::new(cwd.path(), "wait", 1, true, &subagent_type)
                    .with_task_id(Some(task_id)),
            ),
            deps: RuntimeTurnDeps::new(&instructions, &memory, &mcp_registry, &hooks),
            output: RuntimeTurnOutputContext::new(&mut events, &mut sink),
            prepared_conversation: &mut prepared_conversation,
            loop_state,
            policy: RuntimeTurnPolicyContext::new(
                &config,
                AgentToolPolicyContext::unrestricted(),
                &policy,
            ),
            workflow: RuntimeTurnWorkflowContext::new(&mut background_workflows, None),
        };
        park_pending_task_wait(&mut input).unwrap();
        assert!(task_registry.get(task_id).unwrap().pending_wait.is_none());
        let transcript = input
            .prepared_conversation
            .checkpoint_parts()
            .0
            .messages
            .iter()
            .filter_map(|message| match message {
                orca_core::conversation::Message::System { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            transcript.contains("target_reached_terminal"),
            "{transcript}"
        );
    }

    #[test]
    fn thirty_two_parent_loops_release_and_reacquire_leases_while_waiting() {
        let registry = TaskRegistry::new("model-wait-scope".into());
        let limits = orca_core::subagent_config::SubagentLimits::default();
        let parents: Vec<_> = (0..32)
            .map(|i| {
                registry
                    .execution_scope(&limits)
                    .submit_subagent(format!("parent-{i}"), None, None, 1)
                    .unwrap()
                    .task_id
            })
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(33));
        let (done, completed) = std::sync::mpsc::channel();
        let workers: Vec<_> = parents
            .into_iter()
            .map(|parent| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    let child = registry
                        .execution_scope(&limits)
                        .submit_subagent("child".into(), None, Some(parent.clone()), 2)
                        .unwrap();
                    assert_eq!(child.admission, crate::execution_scope::Admission::Queued);
                    let child_id = child.task_id.clone();
                    let child_registry = registry.clone();
                    registry
                        .scope_arbiter()
                        .enqueue(registry.clone(), child.task_id.clone(), limits, move || {
                            assert!(child_registry.execution_scope(&limits).running() <= 32);
                            child_registry
                                .complete_with_usage(&child_id, "done".into(), None)
                                .unwrap();
                        })
                        .unwrap();
                    registry
                        .set_pending_wait(
                            &parent,
                            Some(crate::tasks::PendingTaskWait {
                                tool_call_id: format!("wait-{parent}"),
                                task_ids: vec![child.task_id],
                                deadline_at_ms: chrono::Utc::now().timestamp_millis() + 10_000,
                                any: false,
                                state_change: false,
                                baseline: Vec::new(),
                                result: None,
                            }),
                        )
                        .unwrap();
                    barrier.wait();
                    exercise_pending_wait(&registry, &parent);
                    assert_eq!(
                        registry.get(&parent).unwrap().status,
                        orca_core::task_types::TaskStatus::Running
                    );
                    assert!(registry.execution_scope(&limits).running() <= 32);
                    registry
                        .complete_with_usage(&parent, "integrated".into(), None)
                        .unwrap();
                    done.send(()).unwrap();
                })
            })
            .collect();
        barrier.wait();
        for _ in 0..32 {
            completed
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(registry.execution_scope(&limits).live(), 0);
    }

    #[test]
    fn turn_loop_input_passes_continuation_to_first_iteration_only() {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &config.model_runtime,
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
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = policy_for_tool_execution(&config);
        let subagent_type = SubagentType::General;
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("turn-loop-continuation".to_string());
        let mut cost_tracker = CostTracker::new(None);
        let loop_state =
            RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry).into_loop_state();
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-loop-continuation");
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("turn-loop-continuation".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let mut prepared_conversation = RuntimeConversationBootstrapStep::new().prepare(
            AgentConversationContext::borrowed(&mut conversation, None),
            cwd.path(),
            "continue",
            0,
            &subagent_type,
            &instructions,
            config.approval_mode,
            &memory,
            orca_core::subagent_config::DelegationPolicy::default(),
        );
        let mut background_workflows = Vec::new();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("continued".to_string())],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let continuation = crate::background_turn::RuntimeTurnContinuation {
            response: crate::model_response::RuntimeModelResponse::new(
                response,
                orca_core::thread_identity::TurnId::new(),
            ),
            preapproved_tool_call_id: Some("tool-1".to_string()),
        };
        let mut input = RuntimeTurnLoopInput {
            actor: &mut actor,
            operation: &mut crate::operation_context::OperationContext::for_tests(
                orca_core::budget::BudgetSpec::default(),
                "turn-loop-input-test",
            ),
            provider_context: RuntimeTurnProviderContext::new(
                ProviderKind::DeepSeek,
                &context_config,
                &provider_config,
                &config.model,
            ),
            request: RuntimeTurnRequestContext::new(
                RuntimeTurnContext::new(cwd.path(), "continue", 0, true, &subagent_type)
                    .with_continuation(continuation),
            ),
            deps: RuntimeTurnDeps::new(&instructions, &memory, &mcp_registry, &hooks),
            output: RuntimeTurnOutputContext::new(&mut events, &mut sink),
            prepared_conversation: &mut prepared_conversation,
            loop_state,
            policy: RuntimeTurnPolicyContext::new(
                &config,
                AgentToolPolicyContext::unrestricted(),
                &policy,
            ),
            workflow: RuntimeTurnWorkflowContext::new(&mut background_workflows, None),
        };

        {
            let first_iteration = input.iteration_input(None);
            assert_eq!(
                first_iteration
                    .request
                    .turn_context
                    .continuation
                    .as_ref()
                    .and_then(|continuation| {
                        continuation.response.response.assistant_content.as_deref()
                    }),
                Some("continued")
            );
            assert_eq!(
                first_iteration
                    .request
                    .turn_context
                    .continuation
                    .as_ref()
                    .and_then(|continuation| continuation.preapproved_tool_call_id()),
                Some("tool-1")
            );
        }
        {
            let second_iteration = input.iteration_input(None);
            assert!(second_iteration.request.turn_context.continuation.is_none());
        }
    }
}
