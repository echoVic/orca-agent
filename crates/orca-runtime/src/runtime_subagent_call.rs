use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use orca_core::cancel::CancelToken;
use orca_core::config::{DelegationSnapshot, RunConfig};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventFactory, EventType, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::subagent_types::SubagentType;
use orca_core::thread_identity::TurnId;
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use serde_json::Value;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime, ChildAgentRuntimeContext,
    run_child_agent,
};
use crate::agent_continuation::{
    AgentCheckpoint, AgentCheckpointId, AgentContinuationError, AgentContinuationId, AgentPromptId,
    AgentTerminal, ChildAgentCoordinator, ChildConversationSnapshot, ContinuationCompatibility,
    ContinuationLease, ContinuationProjection, ContinuationRevision, CreateContinuationInput,
    PreparedContinuation, ResumeContinuationInput, WorktreeBinding,
    compute_continuation_compatibility_hash,
};
use crate::child_agent_types::{
    ChildAgentActivityEmitter, ChildAgentActivityPublisher, ChildAgentActivitySink,
    ChildAgentCheckpointObserver, ChildAgentCompatibilityIdentity, ChildAgentContinuationStart,
    SubagentActivityEvent, SubagentActivityIdentity, SubagentActivityOwner,
    SubagentActivityPayload, child_event_output,
};
use crate::child_permission::{ChildPermissionHandler, ChildPermissionIdentity};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskLifecycle, RuntimeTaskStatus,
};
use crate::memory::MemoryBlock;
use crate::runtime_permission::RuntimePermissionRequestHandler;
use crate::runtime_surface::RuntimeSubagentActivityIngress;
use crate::runtime_surface::{
    DisplayText, SurfaceSubagentId, SurfaceSubagentTerminalStatus, SurfaceTaskId, TaskRevision,
};
use crate::runtime_tool_call::RuntimeToolCallRuntime;
use crate::schema_validation::validate_json_schema_subset;
use crate::subagent::{SubagentIsolation, SubagentRequest};
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::worktree::{WorktreeGuard, WorktreeOutcome};

pub(crate) struct RuntimeSubagentInvocation {
    pub(crate) tool_request: ToolRequest,
    pub(crate) request: SubagentRequest,
    pub(crate) config: RunConfig,
    pub(crate) cwd: PathBuf,
    pub(crate) instructions: ProjectInstructions,
    pub(crate) memory: MemoryBlock,
    pub(crate) mcp_registry: McpRegistry,
    pub(crate) hooks: HookRunner,
    pub(crate) workflow_ipc: Option<WorkflowIpcContext>,
    pub(crate) child_depth: u32,
    pub(crate) child_executor: ChildAgentExecutor<io::Sink>,
    pub(crate) activity_ingress: Option<Arc<dyn RuntimeSubagentActivityIngress>>,
    /// Owned parent handler; the child wraps it with its own typed identity
    /// before any tool can request an escalation.
    pub(crate) permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    pub(crate) task_registry: TaskRegistry,
    pub(crate) root_task_id: Option<String>,
}

/// The temporary runtime-side activity bridge. The surface actor will replace
/// this mirror with its ingress, but the child never silently drops an event:
/// an update failure is returned to the execution path before a tool launches.
pub(crate) struct TaskRegistryActivitySink {
    pub(crate) task_registry: TaskRegistry,
    pub(crate) task_id: String,
}

/// Synchronous child delivery boundary. The child retains the source event on
/// failure, so retries present the same commit id and digest to the actor.
pub(crate) struct RuntimeSubagentActivitySink {
    pub(crate) ingress: Arc<dyn RuntimeSubagentActivityIngress>,
}

impl ChildAgentActivitySink for RuntimeSubagentActivitySink {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
        if !event.verify_digest() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child activity digest verification failed",
            ));
        }
        self.ingress.commit_activity(event)
    }
}

impl ChildAgentActivitySink for TaskRegistryActivitySink {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
        if !event.verify_digest() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child activity digest verification failed",
            ));
        }
        let (activity, turn, usage) = match event.payload {
            SubagentActivityPayload::Started { description } => {
                (format!("started: {}", description.as_str()), None, None)
            }
            SubagentActivityPayload::PhaseChanged { phase, turn } => {
                (format!("phase: {phase:?}"), turn, None)
            }
            SubagentActivityPayload::ToolStarted { name, target, .. } => (
                target
                    .as_ref()
                    .map(|target| format!("{name}: {}", target.as_str()))
                    .unwrap_or(name),
                None,
                None,
            ),
            SubagentActivityPayload::ToolCompleted { status, .. } => {
                (format!("tool completed: {status:?}"), None, None)
            }
            SubagentActivityPayload::Usage { totals } => {
                ("usage updated".to_string(), None, Some(totals))
            }
            SubagentActivityPayload::CheckpointPublished {
                checkpoint_revision,
            } => (format!("checkpoint {checkpoint_revision}"), None, None),
            SubagentActivityPayload::Completed { status, .. } => {
                (format!("completed: {status:?}"), None, None)
            }
        };
        self.task_registry
            .update_subagent_activity(&self.task_id, activity, turn, usage)
            .map_err(io::Error::other)
    }
}

pub(crate) struct ChildEventActivityObserver {
    pub(crate) emitter: Arc<ChildAgentActivityEmitter>,
}

impl EventObserver for ChildEventActivityObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        let activity = match event.event_type {
            EventType::TurnStarted => Some(crate::agent_child::ChildAgentActivity::TurnStarted {
                turn: event.payload["turn"].as_u64().unwrap_or_default() as u32,
            }),
            EventType::AssistantReasoningDelta | EventType::AssistantMessageDelta => {
                Some(crate::agent_child::ChildAgentActivity::Streaming)
            }
            EventType::ToolCallRequested => {
                Some(crate::agent_child::ChildAgentActivity::ToolStarted {
                    call_id: event.payload["id"]
                        .as_str()
                        .unwrap_or("unknown-tool-call")
                        .to_string(),
                    name: event.payload["name"].as_str().unwrap_or("tool").to_string(),
                    target: event.payload["target"].as_str().map(str::to_string),
                })
            }
            EventType::ToolCallCompleted => {
                Some(crate::agent_child::ChildAgentActivity::ToolCompleted {
                    call_id: event.payload["id"]
                        .as_str()
                        .unwrap_or("unknown-tool-call")
                        .to_string(),
                    name: event.payload["name"].as_str().unwrap_or("tool").to_string(),
                    status: match event.payload["status"].as_str() {
                        Some("completed") => RunStatus::Success,
                        Some("cancelled") => RunStatus::Cancelled,
                        _ => RunStatus::Failed,
                    },
                })
            }
            EventType::UsageUpdated => {
                Some(crate::agent_child::ChildAgentActivity::Usage(UsageTotals {
                    input_tokens: event.payload["input_tokens"].as_u64().unwrap_or_default(),
                    output_tokens: event.payload["output_tokens"].as_u64().unwrap_or_default(),
                    cache_tokens: event.payload["cache_tokens"].as_u64().unwrap_or_default(),
                    estimated_cost_usd: event.payload["estimated_cost_usd"]
                        .as_f64()
                        .unwrap_or_default(),
                }))
            }
            _ => None,
        };
        if let Some(activity) = activity {
            self.emitter.publish_activity(activity)?;
        }
        Ok(())
    }
}

impl RuntimeSubagentInvocation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn snapshot(
        tool_request: ToolRequest,
        request: SubagentRequest,
        config: &RunConfig,
        cwd: &Path,
        instructions: &ProjectInstructions,
        memory: &MemoryBlock,
        mcp_registry: &McpRegistry,
        hooks: &HookRunner,
        workflow_ipc: Option<&WorkflowIpcContext>,
        child_depth: u32,
        child_executor: ChildAgentExecutor<io::Sink>,
        activity_ingress: Option<Arc<dyn RuntimeSubagentActivityIngress>>,
        permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
        task_registry: &TaskRegistry,
        root_task_id: Option<&str>,
    ) -> Self {
        Self {
            tool_request,
            request,
            config: config.clone(),
            cwd: cwd.to_path_buf(),
            instructions: instructions.clone(),
            memory: memory.clone(),
            mcp_registry: mcp_registry.clone(),
            hooks: hooks.clone(),
            workflow_ipc: workflow_ipc.cloned(),
            child_depth,
            child_executor,
            activity_ingress,
            permission_handler,
            task_registry: task_registry.clone(),
            root_task_id: root_task_id.map(str::to_string),
        }
    }
}

pub(crate) struct RuntimeSubagentCallOutput {
    pub(crate) tool_request: ToolRequest,
    pub(crate) description: String,
    pub(crate) task: Option<RuntimeTaskLifecycle>,
    pub(crate) status: RunStatus,
    pub(crate) result: ToolResult,
    pub(crate) event_output: Option<String>,
    pub(crate) event_error: Option<String>,
    pub(crate) cost_tracker: CostTracker,
    /// The child's consumed budget receipt, when the child loop reported one.
    pub(crate) child_budget_usage: Option<orca_core::budget::BudgetUsage>,
}

pub(crate) struct RuntimeSubagentAdmission {
    pub(crate) immediate: Option<(usize, RuntimeSubagentCallOutput)>,
    pub(crate) event_error: Option<io::Error>,
}

struct RuntimeSubagentWorker {
    index: usize,
    tool_request: ToolRequest,
    description: String,
    started_task: RuntimeTaskLifecycle,
    join: thread::JoinHandle<RuntimeSubagentCallOutput>,
}

pub(crate) struct RuntimeSubagentBatch {
    cancel: CancelToken,
    workers: Vec<RuntimeSubagentWorker>,
}

impl RuntimeToolCallRuntime {
    pub(crate) fn start_subagent_batch(&self, cancel: &CancelToken) -> RuntimeSubagentBatch {
        RuntimeSubagentBatch {
            cancel: cancel.clone(),
            workers: Vec::new(),
        }
    }

    pub(crate) fn execute_subagent(
        &self,
        invocation: RuntimeSubagentInvocation,
        cancel: &CancelToken,
        publish_started: impl FnOnce(&RuntimeTaskLifecycle) -> io::Result<()>,
    ) -> RuntimeSubagentExecution {
        let mut batch = self.start_subagent_batch(cancel);
        let admission = batch.admit(0, invocation, publish_started);
        let mut output = admission.immediate.map(|(_, output)| output);
        let event_error = admission.event_error;
        if let Some((_, completed)) = batch.finish().into_iter().next() {
            output = Some(completed);
        }
        RuntimeSubagentExecution {
            output: output.expect("one subagent invocation must produce one output"),
            event_error,
        }
    }
}

pub(crate) struct RuntimeSubagentExecution {
    pub(crate) output: RuntimeSubagentCallOutput,
    pub(crate) event_error: Option<io::Error>,
}

impl RuntimeSubagentBatch {
    pub(crate) fn admit(
        &mut self,
        index: usize,
        invocation: RuntimeSubagentInvocation,
        publish_started: impl FnOnce(&RuntimeTaskLifecycle) -> io::Result<()>,
    ) -> RuntimeSubagentAdmission {
        if self.cancel.is_cancelled() {
            return RuntimeSubagentAdmission {
                immediate: Some((index, cancelled_before_start(invocation))),
                event_error: None,
            };
        }

        let mut lifecycle =
            RuntimeSessionLifecycle::new(format!("subagent-{}", invocation.tool_request.id));
        let started_task = lifecycle.start_task(RuntimeTaskKind::Subagent).clone();
        if let Err(error) = publish_started(&started_task) {
            return RuntimeSubagentAdmission {
                immediate: Some((
                    index,
                    failed_before_start(
                        invocation,
                        "subagent dispatch stopped because its started event could not be delivered",
                    ),
                )),
                event_error: Some(error),
            };
        }

        let tool_request = invocation.tool_request.clone();
        let description = invocation.request.description.clone();
        let panic_request = tool_request.clone();
        let panic_description = description.clone();
        let panic_task = started_task.clone();
        let worker_cancel = self.cancel.clone();
        let join = match thread::Builder::new()
            .name(format!("orca-subagent-{}", tool_request.id))
            .spawn(move || run_subagent_worker(invocation, lifecycle, started_task, worker_cancel))
        {
            Ok(join) => join,
            Err(error) => {
                let message = format!("failed to start subagent worker: {error}");
                return RuntimeSubagentAdmission {
                    immediate: Some((
                        index,
                        RuntimeSubagentCallOutput {
                            tool_request: panic_request.clone(),
                            description: panic_description,
                            task: Some(panic_task.with_status(RuntimeTaskStatus::Failed)),
                            status: RunStatus::Failed,
                            result: ToolResult::failed_before_start(&panic_request, &message, None),
                            event_output: None,
                            event_error: Some(message),
                            cost_tracker: CostTracker::new(None),
                            child_budget_usage: None,
                        },
                    )),
                    event_error: None,
                };
            }
        };
        self.workers.push(RuntimeSubagentWorker {
            index,
            tool_request,
            description,
            started_task: panic_task,
            join,
        });
        RuntimeSubagentAdmission {
            immediate: None,
            event_error: None,
        }
    }

    pub(crate) fn finish(self) -> Vec<(usize, RuntimeSubagentCallOutput)> {
        self.workers
            .into_iter()
            .map(|worker| {
                let output = match worker.join.join() {
                    Ok(output) => output,
                    Err(payload) => {
                        let error = format!(
                            "Subagent worker panicked after execution started: {}. Inspect external state before retrying.",
                            panic_payload_message(payload)
                        );
                        RuntimeSubagentCallOutput {
                            result: ToolResult::indeterminate_after_start(
                                &worker.tool_request,
                                &error,
                            ),
                            tool_request: worker.tool_request,
                            description: worker.description,
                            task: Some(
                                worker
                                    .started_task
                                    .with_status(RuntimeTaskStatus::Failed),
                            ),
                            status: RunStatus::Failed,
                            event_output: None,
                            event_error: Some(error),
                            cost_tracker: CostTracker::new(None),
            child_budget_usage: None,
                        }
                    }
                };
                (worker.index, output)
            })
            .collect()
    }
}

fn run_subagent_worker(
    invocation: RuntimeSubagentInvocation,
    lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    cancel: CancelToken,
) -> RuntimeSubagentCallOutput {
    let RuntimeSubagentInvocation {
        tool_request,
        request,
        config,
        cwd,
        instructions,
        memory,
        mcp_registry,
        hooks,
        workflow_ipc,
        child_depth,
        child_executor,
        activity_ingress,
        permission_handler,
        task_registry,
        root_task_id,
    } = invocation;
    let SubagentRequest {
        description,
        prompt,
        subagent_type: requested_subagent_type,
        model: requested_model,
        mode: _,
        isolation: requested_isolation,
        schema,
        resume_from,
        delegation,
    } = request;
    let delegation = delegation.unwrap_or_else(|| DelegationSnapshot::from_config(&config));
    let coordinator_result = ChildAgentCoordinator::new(task_registry.clone());
    let source_result = match (&coordinator_result, resume_from.as_deref()) {
        (Ok(coordinator), Some(selector)) => coordinator.prepared(selector).map(Some),
        (Err(error), _) => Err(error.clone()),
        (_, None) => Ok(None),
    };
    let task_agent_type = source_result
        .as_ref()
        .ok()
        .and_then(|source| source.as_ref())
        .map(|source| source.compatibility.subagent_type.clone())
        .or_else(|| serialized_subagent_type(&requested_subagent_type));
    let registry_task = task_registry.create_subagent_with_parent(
        description.clone(),
        task_agent_type,
        root_task_id.clone(),
    );
    let registry_task_id = registry_task.id.clone();
    if let Err(error) = task_registry.mark_running(&registry_task_id) {
        return sync_setup_failure(
            tool_request,
            description,
            lifecycle,
            started_task,
            &config,
            &task_registry,
            &registry_task_id,
            format!("failed to mark synchronous subagent task running: {error}"),
        );
    }
    let coordinator = match coordinator_result {
        Ok(coordinator) => coordinator,
        Err(error) => {
            return sync_setup_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                continuation_error("failed to initialize child continuation", &error),
            );
        }
    };
    let source = match source_result {
        Ok(source) => source,
        Err(error) => {
            return sync_setup_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                continuation_error("failed to resolve child continuation", &error),
            );
        }
    };
    if let Some(source) = source.as_ref()
        && let Err(error) = validate_resume_overrides(
            &tool_request,
            &requested_subagent_type,
            requested_model.as_deref(),
            requested_isolation,
            source,
        )
    {
        return sync_setup_failure(
            tool_request,
            description,
            lifecycle,
            started_task,
            &config,
            &task_registry,
            &registry_task_id,
            error,
        );
    }

    let subagent_type = source
        .as_ref()
        .map(|source| SubagentType::from_str(&source.compatibility.subagent_type))
        .unwrap_or(requested_subagent_type);
    let model = source
        .as_ref()
        .map(|source| source.compatibility.model.clone())
        .unwrap_or(requested_model);
    let isolation = source
        .as_ref()
        .map(|source| source.compatibility.isolation)
        .unwrap_or(requested_isolation);
    let mut child_config = config.clone();
    delegation.apply_to(&mut child_config, model.clone());
    let effective_model = child_config.model.as_option();

    let worktree_execution = match prepare_sync_worktree(source.as_ref(), isolation, &cwd) {
        Ok(worktree) => worktree,
        Err(error) => {
            return sync_setup_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                error,
            );
        }
    };
    let child_cwd = worktree_execution.cwd().to_path_buf();
    let worktree_binding = worktree_execution.binding();
    let effective_cwd = child_cwd.display().to_string();
    let compatibility_hash = match compute_continuation_compatibility_hash(
        &subagent_type,
        effective_model.as_deref(),
        isolation,
        &effective_cwd,
        worktree_binding.as_ref(),
        &delegation,
        &mcp_registry,
        &child_config.external_tools,
    ) {
        Ok(hash) => hash,
        Err(error) => {
            let worktree = worktree_execution.finish();
            return sync_setup_failure_with_worktree(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                continuation_error("failed to compute child compatibility", &error),
                worktree,
            );
        }
    };
    let compatibility = ContinuationCompatibility {
        subagent_type: serialized_subagent_type(&subagent_type)
            .unwrap_or_else(|| "general".to_string()),
        model: effective_model.clone(),
        isolation,
        effective_cwd,
        worktree: worktree_binding,
        compatibility_hash,
    };
    let prompt_id = AgentPromptId::new();
    let prepared = match source.as_ref() {
        Some(_) => {
            let input = ResumeContinuationInput {
                selector: resume_from.expect("resume source requires selector"),
                parent_task_id: root_task_id.clone(),
                task_id: registry_task_id.clone(),
                prompt_id,
                compatibility,
            };
            coordinator
                .prepare_resume(input.clone())
                .or_else(|_| coordinator.prepare_resume(input))
        }
        None => {
            let input = CreateContinuationInput {
                continuation_id: Some(AgentContinuationId::new()),
                parent_task_id: root_task_id.clone(),
                task_id: registry_task_id.clone(),
                prompt_id,
                compatibility,
            };
            coordinator
                .create(input.clone())
                .or_else(|_| coordinator.create(input))
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let worktree = worktree_execution.finish();
            return sync_setup_failure_with_worktree(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                continuation_error("failed to prepare child continuation", &error),
                worktree,
            );
        }
    };
    let lease = match coordinator
        .acquire(&prepared)
        .or_else(|_| coordinator.acquire(&prepared))
    {
        Ok(lease) => lease,
        Err(error) => {
            let worktree = worktree_execution.finish();
            return sync_setup_failure_with_worktree(
                tool_request,
                description,
                lifecycle,
                started_task,
                &config,
                &task_registry,
                &registry_task_id,
                continuation_error("failed to acquire child continuation", &error),
                worktree,
            );
        }
    };
    let panic_request = tool_request.clone();
    let panic_description = description.clone();
    let panic_task = started_task.clone();
    let panic_model = config.model.as_option();
    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        execute_acquired_sync_subagent(
            tool_request,
            description,
            prompt,
            subagent_type,
            effective_model,
            schema,
            source.is_some(),
            prepared,
            coordinator.clone(),
            lease.clone(),
            child_config,
            child_cwd,
            worktree_execution,
            instructions,
            memory,
            mcp_registry,
            hooks,
            workflow_ipc,
            child_depth,
            child_executor,
            activity_ingress,
            permission_handler,
            task_registry.clone(),
            root_task_id,
            lifecycle,
            started_task,
            cancel,
            registry_task_id.clone(),
            config.output_format,
        )
    }));
    match execution {
        Ok(output) => output,
        Err(payload) => {
            let error = format!(
                "Subagent worker panicked after continuation acquisition: {}. Inspect external state before retrying.",
                panic_payload_message(payload)
            );
            let output = RuntimeSubagentCallOutput {
                result: ToolResult::indeterminate_after_start(&panic_request, &error),
                tool_request: panic_request,
                description: panic_description,
                task: Some(panic_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(error),
                cost_tracker: CostTracker::new(panic_model.as_deref()),
                child_budget_usage: None,
            };
            finalize_started_sync_subagent(
                output,
                &coordinator,
                &lease,
                &task_registry,
                &registry_task_id,
                true,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_acquired_sync_subagent(
    tool_request: ToolRequest,
    description: String,
    prompt: String,
    subagent_type: SubagentType,
    effective_model: Option<String>,
    schema: Option<Value>,
    is_resume: bool,
    prepared: PreparedContinuation,
    coordinator: ChildAgentCoordinator,
    lease: ContinuationLease,
    child_config: RunConfig,
    child_cwd: PathBuf,
    worktree_execution: SyncWorktreeExecution,
    instructions: ProjectInstructions,
    memory: MemoryBlock,
    mcp_registry: McpRegistry,
    hooks: HookRunner,
    workflow_ipc: Option<WorkflowIpcContext>,
    child_depth: u32,
    child_executor: ChildAgentExecutor<io::Sink>,
    activity_ingress: Option<Arc<dyn RuntimeSubagentActivityIngress>>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    task_registry: TaskRegistry,
    root_task_id: Option<String>,
    mut lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    cancel: CancelToken,
    registry_task_id: String,
    output_format: orca_core::config::OutputFormat,
) -> RuntimeSubagentCallOutput {
    let continuation_start = if is_resume {
        let Some(checkpoint) = prepared.checkpoint.clone() else {
            let output = continuation_started_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &child_config,
                "continuation_checkpoint_missing: resume source has no safe checkpoint".to_string(),
                worktree_execution.finish(),
            );
            return finalize_started_sync_subagent(
                output,
                &coordinator,
                &lease,
                &task_registry,
                &registry_task_id,
                false,
            );
        };
        match ChildAgentContinuationStart::new(
            prepared.continuation_id.clone(),
            prepared.attempt_id.clone(),
            prepared.prompt_id.clone(),
            checkpoint,
            ChildAgentCompatibilityIdentity::new(prepared.compatibility.compatibility_hash),
        ) {
            Ok(start) => Some(start),
            Err(error) => {
                let output = continuation_started_failure(
                    tool_request,
                    description,
                    lifecycle,
                    started_task,
                    &child_config,
                    continuation_error("failed to construct child continuation start", &error),
                    worktree_execution.finish(),
                );
                return finalize_started_sync_subagent(
                    output,
                    &coordinator,
                    &lease,
                    &task_registry,
                    &registry_task_id,
                    false,
                );
            }
        }
    } else {
        None
    };
    let (checkpoint_observer, shared_revision) =
        build_checkpoint_observer(coordinator.clone(), lease.clone(), &prepared);
    let child_request = ChildAgentRequest {
        prompt,
        subagent_type,
        model: effective_model,
        depth: child_depth,
        emit_deltas: true,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc,
        continuation: continuation_start,
    };
    let surface_task_id = match SurfaceTaskId::try_new(registry_task_id.clone()) {
        Ok(task_id) => task_id,
        Err(error) => {
            let output = continuation_started_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &child_config,
                format!("child permission identity unavailable: invalid task id ({error})"),
                worktree_execution.finish(),
            );
            return finalize_started_sync_subagent(
                output,
                &coordinator,
                &lease,
                &task_registry,
                &registry_task_id,
                false,
            );
        }
    };
    let surface_subagent_id = match SurfaceSubagentId::try_new(tool_request.id.clone()) {
        Ok(subagent_id) => subagent_id,
        Err(error) => {
            let output = continuation_started_failure(
                tool_request,
                description,
                lifecycle,
                started_task,
                &child_config,
                format!("child permission identity unavailable: invalid subagent id ({error})"),
                worktree_execution.finish(),
            );
            return finalize_started_sync_subagent(
                output,
                &coordinator,
                &lease,
                &task_registry,
                &registry_task_id,
                false,
            );
        }
    };
    let activity_owner = activity_ingress.as_ref().map_or_else(
        || SubagentActivityOwner::DetachedTask {
            task_id: surface_task_id.clone(),
            task_revision: TaskRevision::try_new(1).expect("one is a valid task revision"),
            authority_digest: prepared.compatibility.compatibility_hash,
        },
        |ingress| ingress.owner(),
    );
    let activity_sink: Arc<dyn ChildAgentActivitySink> = match activity_ingress {
        Some(ingress) => Arc::new(RuntimeSubagentActivitySink { ingress }),
        None => Arc::new(TaskRegistryActivitySink {
            task_registry: task_registry.clone(),
            task_id: registry_task_id.clone(),
        }),
    };
    let activity = Arc::new(ChildAgentActivityEmitter::new(
        SubagentActivityIdentity {
            task_id: surface_task_id.clone(),
            subagent_id: surface_subagent_id.clone(),
            attempt_id: prepared.attempt_id.clone(),
            owner: activity_owner,
        },
        activity_sink,
    ));
    if let Err(error) = activity.publish_payload(SubagentActivityPayload::Started {
        description: DisplayText::new(&description),
    }) {
        let output = continuation_started_failure(
            tool_request,
            description,
            lifecycle,
            started_task,
            &child_config,
            format!("child activity start could not be durably published: {error}"),
            worktree_execution.finish(),
        );
        return finalize_started_sync_subagent(
            output,
            &coordinator,
            &lease,
            &task_registry,
            &registry_task_id,
            false,
        );
    }
    let child_turn_id = TurnId::new();
    let child_permission_handler = permission_handler.map(|parent| {
        Arc::new(ChildPermissionHandler::new(
            parent,
            ChildPermissionIdentity::new(
                surface_task_id.clone(),
                surface_subagent_id,
                child_turn_id.clone(),
                activity.revision_source(),
            ),
        )) as Arc<dyn RuntimePermissionRequestHandler + Send + Sync>
    });
    let mut child_events = EventFactory::new(format!("subagent-{}", tool_request.id));
    let mut child_sink = EventSink::new(child_event_output(), output_format).with_observer(
        Arc::new(ChildEventActivityObserver {
            emitter: Arc::clone(&activity),
        }),
    );
    let child = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
            cwd: &child_cwd,
            events: &mut child_events,
            sink: &mut child_sink,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &mcp_registry,
            hooks: &hooks,
            cancel: &cancel,
            lifecycle: Some(&mut lifecycle),
            task_registry: Some(&task_registry),
            root_task_id: root_task_id.as_deref(),
            checkpoint_observer: Some(&checkpoint_observer),
            permission_handler: child_permission_handler,
            turn_id: Some(child_turn_id),
            executor: child_executor,
        });
        run_child_agent(&child_config, &child_request, &mut runtime)
    }));
    let worktree = worktree_execution.finish();
    let (mut output, panicked) = match child {
        Ok((child, cost_tracker)) => (
            finish_child_output(
                tool_request,
                description,
                schema.as_ref(),
                child,
                cost_tracker,
                worktree,
                lifecycle,
                started_task,
            ),
            false,
        ),
        Err(payload) => {
            let mut error = format!(
                "Subagent worker panicked after execution started: {}. Inspect external state before retrying.",
                panic_payload_message(payload)
            );
            match worktree {
                Ok(worktree) => append_worktree_outcome(&mut error, worktree.as_ref()),
                Err(cleanup_error) => error.push_str(&format!(
                    "\n\nFailed to finish subagent worktree after panic: {cleanup_error}"
                )),
            }
            let task = lifecycle
                .finish_task(RunStatus::Failed)
                .cloned()
                .unwrap_or_else(|| started_task.with_status(RuntimeTaskStatus::Failed));
            (
                RuntimeSubagentCallOutput {
                    result: ToolResult::indeterminate_after_start(&tool_request, &error),
                    tool_request,
                    description,
                    task: Some(task),
                    status: RunStatus::Failed,
                    event_output: None,
                    event_error: Some(error),
                    cost_tracker: CostTracker::new(child_config.model.as_deref()),
                    child_budget_usage: None,
                },
                true,
            )
        }
    };
    let terminal_status = match output.status {
        RunStatus::Success => SurfaceSubagentTerminalStatus::Completed,
        RunStatus::Cancelled => SurfaceSubagentTerminalStatus::Cancelled,
        RunStatus::Failed | RunStatus::ApprovalRequired | RunStatus::VerificationFailed => {
            SurfaceSubagentTerminalStatus::Failed
        }
    };
    if let Err(error) = activity.publish_payload(SubagentActivityPayload::Completed {
        status: terminal_status,
        output: output.event_output.as_deref().map(DisplayText::new),
        error: output.event_error.as_deref().map(DisplayText::new),
        usage: Some(output.cost_tracker.totals()),
    }) {
        let message = format!("child activity terminal could not be durably published: {error}");
        output.event_error = Some(match output.event_error.take() {
            Some(existing) => format!("{existing}\n\n{message}"),
            None => message,
        });
        output.status = RunStatus::Failed;
    }
    finalize_started_sync_subagent_with_revision(
        output,
        &coordinator,
        &lease,
        &shared_revision,
        &task_registry,
        &registry_task_id,
        panicked,
    )
}

enum SyncWorktreeExecution {
    Plain(PathBuf),
    Fresh(WorktreeGuard),
    Inherited(WorktreeBinding),
}

impl SyncWorktreeExecution {
    fn cwd(&self) -> &Path {
        match self {
            Self::Plain(path) => path,
            Self::Fresh(guard) => guard.path(),
            Self::Inherited(binding) => Path::new(&binding.path),
        }
    }

    fn binding(&self) -> Option<WorktreeBinding> {
        match self {
            Self::Plain(_) => None,
            Self::Fresh(guard) => Some(WorktreeBinding {
                repo_root: guard.repo_root().display().to_string(),
                path: guard.path().display().to_string(),
            }),
            Self::Inherited(binding) => Some(binding.clone()),
        }
    }

    fn finish(self) -> io::Result<Option<WorktreeOutcome>> {
        match self {
            Self::Plain(_) => Ok(None),
            Self::Fresh(guard) => guard.finish().map(Some),
            Self::Inherited(binding) => Ok(Some(WorktreeOutcome {
                path: PathBuf::from(binding.path),
                preserved: true,
            })),
        }
    }
}

fn prepare_sync_worktree(
    source: Option<&PreparedContinuation>,
    isolation: SubagentIsolation,
    parent_cwd: &Path,
) -> Result<SyncWorktreeExecution, String> {
    if let Some(source) = source {
        let effective_cwd = PathBuf::from(&source.compatibility.effective_cwd);
        if !effective_cwd.is_dir() {
            return Err(
                "continuation_incompatible: inherited effective cwd is missing or not a directory"
                    .to_string(),
            );
        }
        return match isolation {
            SubagentIsolation::None => Ok(SyncWorktreeExecution::Plain(effective_cwd)),
            SubagentIsolation::Worktree => source
                .compatibility
                .worktree
                .clone()
                .map(SyncWorktreeExecution::Inherited)
                .ok_or_else(|| {
                    "continuation_incompatible: worktree continuation has no durable binding"
                        .to_string()
                }),
        };
    }

    match isolation {
        SubagentIsolation::None => Ok(SyncWorktreeExecution::Plain(parent_cwd.to_path_buf())),
        SubagentIsolation::Worktree => WorktreeGuard::create(parent_cwd)
            .map(SyncWorktreeExecution::Fresh)
            .map_err(|error| format!("failed to create subagent worktree: {error}")),
    }
}

pub(crate) fn serialized_subagent_type(subagent_type: &SubagentType) -> Option<String> {
    Some(match subagent_type {
        SubagentType::General => "general".to_string(),
        SubagentType::CodeReviewer => "code_reviewer".to_string(),
        SubagentType::TestWriter => "test_writer".to_string(),
        SubagentType::Debugger => "debugger".to_string(),
        SubagentType::Documenter => "documenter".to_string(),
        SubagentType::Custom(value) => value.clone(),
    })
}

pub(crate) fn validate_resume_overrides(
    tool_request: &ToolRequest,
    requested_subagent_type: &SubagentType,
    requested_model: Option<&str>,
    requested_isolation: SubagentIsolation,
    source: &PreparedContinuation,
) -> Result<(), String> {
    let raw = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    let arguments = serde_json::from_str::<Value>(raw).map_err(|_| {
        "continuation_incompatible: resume arguments are not valid JSON".to_string()
    })?;
    let arguments = arguments.as_object().ok_or_else(|| {
        "continuation_incompatible: resume arguments must be a JSON object".to_string()
    })?;
    if arguments.contains_key("subagent_type")
        && serialized_subagent_type(requested_subagent_type).as_deref()
            != Some(source.compatibility.subagent_type.as_str())
    {
        return Err(
            "continuation_incompatible: explicit subagent_type conflicts with the source continuation"
                .to_string(),
        );
    }
    if arguments.contains_key("model") && requested_model != source.compatibility.model.as_deref() {
        return Err(
            "continuation_incompatible: explicit model conflicts with the source continuation"
                .to_string(),
        );
    }
    if arguments.contains_key("isolation") && requested_isolation != source.compatibility.isolation
    {
        return Err(
            "continuation_incompatible: explicit isolation conflicts with the source continuation"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn build_checkpoint_observer(
    coordinator: ChildAgentCoordinator,
    lease: ContinuationLease,
    prepared: &PreparedContinuation,
) -> (
    ChildAgentCheckpointObserver<'static>,
    Arc<Mutex<ContinuationRevision>>,
) {
    let shared_revision = Arc::new(Mutex::new(lease.revision));
    let observer_revision = Arc::clone(&shared_revision);
    let boundary_revision = Arc::clone(&shared_revision);
    let boundary_coordinator = coordinator.clone();
    let boundary_lease = lease.clone();
    let base_turn = prepared
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.turn)
        .unwrap_or(0);
    let base_usage = prepared
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.usage)
        .unwrap_or_default();
    let mut next_sequence = prepared
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.sequence.saturating_add(1))
        .unwrap_or(0);
    let mut pending_checkpoint: Option<AgentCheckpoint> = None;
    let attempt_id = prepared.attempt_id.clone();
    let observer = ChildAgentCheckpointObserver::new_with_tool_boundary(
        move |observation| {
            let checkpoint = if let Some(checkpoint) = pending_checkpoint.clone() {
                checkpoint
            } else {
                let turn = base_turn.checked_add(observation.turn).ok_or_else(|| {
                    AgentContinuationError::CorruptRecord {
                        message: "continuation checkpoint turn is exhausted".to_string(),
                    }
                })?;
                let next_turn =
                    turn.checked_add(1)
                        .ok_or_else(|| AgentContinuationError::CorruptRecord {
                            message: "continuation checkpoint next turn is exhausted".to_string(),
                        })?;
                let (conversation, captured_boundary) =
                    ChildConversationSnapshot::try_capture_safe(
                        observation.conversation,
                        next_turn,
                    )?;
                if captured_boundary != observation.last_tool_boundary {
                    return Err(AgentContinuationError::CorruptRecord {
                        message: "child checkpoint boundary changed during capture".to_string(),
                    });
                }
                let mut usage = base_usage;
                usage.merge(observation.usage);
                let mut checkpoint = AgentCheckpoint {
                    checkpoint_id: AgentCheckpointId::new(),
                    attempt_id: attempt_id.clone(),
                    sequence: next_sequence,
                    conversation,
                    turn,
                    usage,
                    last_tool_boundary: captured_boundary,
                    created_at_ms: unix_time_ms(),
                    digest: crate::runtime_surface::Sha256Digest::new([0; 32]),
                };
                checkpoint.digest = checkpoint.computed_digest()?;
                pending_checkpoint = Some(checkpoint.clone());
                checkpoint
            };
            let mut expected_revision =
                observer_revision
                    .lock()
                    .map_err(|_| AgentContinuationError::Persistence {
                        message: "continuation checkpoint revision lock is poisoned".to_string(),
                    })?;
            let projection = commit_continuation_write_with_retry(
                &coordinator,
                &lease,
                *expected_revision,
                |revision| coordinator.commit_checkpoint(&lease, revision, checkpoint.clone()),
            )?;
            *expected_revision = projection.revision;
            next_sequence = projection
                .checkpoint_sequence
                .and_then(|sequence| sequence.checked_add(1))
                .ok_or_else(|| AgentContinuationError::CorruptRecord {
                    message: "continuation checkpoint sequence is exhausted".to_string(),
                })?;
            pending_checkpoint = None;
            Ok(())
        },
        move |boundary| {
            let mut expected_revision =
                boundary_revision
                    .lock()
                    .map_err(|_| AgentContinuationError::Persistence {
                        message: "continuation tool-boundary revision lock is poisoned".to_string(),
                    })?;
            let projection = commit_continuation_write_with_retry(
                &boundary_coordinator,
                &boundary_lease,
                *expected_revision,
                |revision| {
                    boundary_coordinator.commit_tool_boundary(
                        &boundary_lease,
                        revision,
                        boundary.clone(),
                    )
                },
            )?;
            *expected_revision = projection.revision;
            Ok(())
        },
    );
    (observer, shared_revision)
}

pub(crate) fn commit_continuation_write_with_retry<F>(
    coordinator: &ChildAgentCoordinator,
    lease: &ContinuationLease,
    expected_revision: ContinuationRevision,
    mut commit: F,
) -> Result<ContinuationProjection, AgentContinuationError>
where
    F: FnMut(ContinuationRevision) -> Result<ContinuationProjection, AgentContinuationError>,
{
    match commit(expected_revision) {
        Ok(projection) => Ok(projection),
        Err(AgentContinuationError::RevisionConflict { .. }) => {
            let refreshed_revision = coordinator
                .projection(lease.continuation_id.as_str())?
                .revision;
            commit(refreshed_revision)
        }
        Err(error) => Err(error),
    }
}

fn finalize_started_sync_subagent(
    output: RuntimeSubagentCallOutput,
    coordinator: &ChildAgentCoordinator,
    lease: &ContinuationLease,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    panicked: bool,
) -> RuntimeSubagentCallOutput {
    let shared_revision = Arc::new(Mutex::new(lease.revision));
    finalize_started_sync_subagent_with_revision(
        output,
        coordinator,
        lease,
        &shared_revision,
        task_registry,
        registry_task_id,
        panicked,
    )
}

fn finalize_started_sync_subagent_with_revision(
    mut output: RuntimeSubagentCallOutput,
    coordinator: &ChildAgentCoordinator,
    lease: &ContinuationLease,
    shared_revision: &Arc<Mutex<ContinuationRevision>>,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    panicked: bool,
) -> RuntimeSubagentCallOutput {
    let expected_revision = coordinator
        .projection(lease.continuation_id.as_str())
        .map(|projection| projection.revision)
        .or_else(|_| {
            shared_revision
                .lock()
                .map(|revision| *revision)
                .map_err(|_| AgentContinuationError::Persistence {
                    message: "continuation terminal revision lock is poisoned".to_string(),
                })
        });
    let terminal = continuation_terminal(&output, panicked);
    let projection = expected_revision.and_then(|revision| {
        commit_continuation_write_with_retry(coordinator, lease, revision, |revision| {
            coordinator.commit_terminal(lease, revision, terminal.clone())
        })
    });
    let projection = match projection {
        Ok(projection) => projection,
        Err(error) => {
            let message =
                continuation_error("failed to commit child continuation terminal", &error);
            let footer_projection = coordinator.projection(lease.continuation_id.as_str()).ok();
            output.result = ToolResult::indeterminate_after_start(&output.tool_request, &message);
            output.status = RunStatus::Failed;
            output.task = output
                .task
                .take()
                .map(|task| task.with_status(RuntimeTaskStatus::Failed));
            output.event_error = Some(message);
            if let Some(projection) = footer_projection.as_ref() {
                append_continuation_footer(&mut output, projection);
            }
            settle_registry_task(
                &mut output,
                task_registry,
                registry_task_id,
                footer_projection.as_ref(),
            );
            return output;
        }
    };
    append_continuation_footer(&mut output, &projection);
    settle_registry_task(
        &mut output,
        task_registry,
        registry_task_id,
        Some(&projection),
    );
    output
}

fn continuation_terminal(output: &RuntimeSubagentCallOutput, panicked: bool) -> AgentTerminal {
    if panicked {
        return AgentTerminal::Indeterminate {
            reason: output
                .event_error
                .clone()
                .unwrap_or_else(|| "subagent panicked after execution started".to_string()),
        };
    }
    match output.status {
        RunStatus::Success => AgentTerminal::Completed {
            result: output.event_output.clone(),
        },
        RunStatus::Cancelled => AgentTerminal::Cancelled {
            reason: output.event_error.clone(),
        },
        _ => AgentTerminal::Failed {
            error: output
                .event_error
                .clone()
                .unwrap_or_else(|| "subagent failed".to_string()),
        },
    }
}

fn append_continuation_footer(
    output: &mut RuntimeSubagentCallOutput,
    projection: &ContinuationProjection,
) {
    let footer = continuation_footer(projection);
    if let Some(result_output) = output.result.output.as_mut() {
        result_output.push_str("\n\n");
        result_output.push_str(&footer);
    } else {
        output.result.append_error(&format!("\n\n{footer}"));
    }
    match output.status {
        RunStatus::Success => {
            let event_output = output.event_output.get_or_insert_with(String::new);
            event_output.push_str("\n\n");
            event_output.push_str(&footer);
        }
        _ => {
            let event_error = output.event_error.get_or_insert_with(String::new);
            event_error.push_str("\n\n");
            event_error.push_str(&footer);
        }
    }
}

pub(crate) fn continuation_footer(projection: &ContinuationProjection) -> String {
    format!(
        "[agent_continuation]\nresume_from={}\nattempt_id={}\ncheckpoint_id={}",
        projection.continuation_id,
        projection.attempt_id,
        projection
            .checkpoint_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    )
}

fn settle_registry_task(
    output: &mut RuntimeSubagentCallOutput,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    footer_projection: Option<&ContinuationProjection>,
) {
    let usage = Some(output.cost_tracker.totals());
    let settlement = match output.status {
        RunStatus::Success => task_registry.complete_with_usage(
            registry_task_id,
            output
                .event_output
                .clone()
                .unwrap_or_else(|| "subagent completed".to_string()),
            usage,
        ),
        RunStatus::Cancelled => task_registry.stop_with_usage(
            registry_task_id,
            output
                .event_error
                .clone()
                .unwrap_or_else(|| "subagent cancelled".to_string()),
            usage,
        ),
        _ => task_registry.fail_with_usage(
            registry_task_id,
            output
                .event_error
                .clone()
                .unwrap_or_else(|| "subagent failed".to_string()),
            usage,
        ),
    };
    if let Err(error) = settlement {
        let mut message = format!(
            "synchronous subagent continuation settled but task registry settlement failed: {error}"
        );
        if let Some(projection) = footer_projection {
            message.push_str("\n\n");
            message.push_str(&continuation_footer(projection));
        }
        output.result = ToolResult::failed_after_start(&output.tool_request, &message, None);
        output.status = RunStatus::Failed;
        output.task = output
            .task
            .take()
            .map(|task| task.with_status(RuntimeTaskStatus::Failed));
        output.event_error = Some(message);
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_setup_failure(
    tool_request: ToolRequest,
    description: String,
    mut lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    config: &RunConfig,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    mut error: String,
) -> RuntimeSubagentCallOutput {
    if let Err(settlement_error) = task_registry.fail(registry_task_id, error.clone()) {
        error.push_str(&format!(
            "\n\nTask registry settlement also failed: {settlement_error}"
        ));
    }
    let task = lifecycle
        .finish_task(RunStatus::Failed)
        .cloned()
        .unwrap_or_else(|| started_task.with_status(RuntimeTaskStatus::Failed));
    RuntimeSubagentCallOutput {
        result: ToolResult::failed_after_start(&tool_request, &error, None),
        tool_request,
        description,
        task: Some(task.with_status(RuntimeTaskStatus::Failed)),
        status: RunStatus::Failed,
        event_output: None,
        event_error: Some(error),
        cost_tracker: CostTracker::new(config.model.as_deref()),
        child_budget_usage: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_setup_failure_with_worktree(
    tool_request: ToolRequest,
    description: String,
    lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    config: &RunConfig,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    mut error: String,
    worktree: io::Result<Option<WorktreeOutcome>>,
) -> RuntimeSubagentCallOutput {
    match worktree {
        Ok(worktree) => append_worktree_outcome(&mut error, worktree.as_ref()),
        Err(cleanup_error) => error.push_str(&format!(
            "\n\nFailed to finish subagent worktree: {cleanup_error}"
        )),
    }
    sync_setup_failure(
        tool_request,
        description,
        lifecycle,
        started_task,
        config,
        task_registry,
        registry_task_id,
        error,
    )
}

fn continuation_started_failure(
    tool_request: ToolRequest,
    description: String,
    mut lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    config: &RunConfig,
    mut error: String,
    worktree: io::Result<Option<WorktreeOutcome>>,
) -> RuntimeSubagentCallOutput {
    match worktree {
        Ok(worktree) => append_worktree_outcome(&mut error, worktree.as_ref()),
        Err(cleanup_error) => error.push_str(&format!(
            "\n\nFailed to finish subagent worktree: {cleanup_error}"
        )),
    }
    let task = lifecycle
        .finish_task(RunStatus::Failed)
        .cloned()
        .unwrap_or_else(|| started_task.with_status(RuntimeTaskStatus::Failed));
    RuntimeSubagentCallOutput {
        result: ToolResult::failed_after_start(&tool_request, &error, None),
        tool_request,
        description,
        task: Some(task.with_status(RuntimeTaskStatus::Failed)),
        status: RunStatus::Failed,
        event_output: None,
        event_error: Some(error),
        cost_tracker: CostTracker::new(config.model.as_deref()),
        child_budget_usage: None,
    }
}

pub(crate) fn continuation_error(context: &str, error: &AgentContinuationError) -> String {
    format!("{} [{}]: {error}", context, error.contract_code())
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn finish_child_output(
    tool_request: ToolRequest,
    description: String,
    schema: Option<&Value>,
    child: crate::agent_child::ChildAgentResult,
    cost_tracker: CostTracker,
    worktree: io::Result<Option<WorktreeOutcome>>,
    mut lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
) -> RuntimeSubagentCallOutput {
    let completed_task = lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| started_task.clone());
    let worktree = match worktree {
        Ok(worktree) => worktree,
        Err(cleanup_error) => {
            let mut error = format!(
                "failed to finish subagent worktree after child status {:?}: {cleanup_error}",
                child.status
            );
            if let Some(child_error) = child.error.as_deref() {
                error.push_str(&format!("\n\nChild error: {child_error}"));
            }
            return RuntimeSubagentCallOutput {
                result: ToolResult::failed_after_start(
                    &tool_request,
                    format!("Subagent status: Failed\n\n{error}"),
                    None,
                ),
                tool_request,
                description,
                task: Some(completed_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: child.final_message,
                event_error: Some(error),
                cost_tracker,
                child_budget_usage: child.budget_usage,
            };
        }
    };

    match child.status {
        RunStatus::Success => {
            let mut output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            if let Err(mut error) = validate_subagent_output_schema(&description, schema, &output) {
                append_worktree_outcome(&mut error, worktree.as_ref());
                return RuntimeSubagentCallOutput {
                    result: ToolResult::failed_after_start(
                        &tool_request,
                        format!("Subagent status: Failed\n\n{error}"),
                        None,
                    ),
                    tool_request,
                    description,
                    task: Some(completed_task.with_status(RuntimeTaskStatus::Failed)),
                    status: RunStatus::Failed,
                    event_output: Some(output),
                    event_error: Some(error),
                    cost_tracker,
                    child_budget_usage: child.budget_usage,
                };
            }
            append_worktree_outcome(&mut output, worktree.as_ref());
            RuntimeSubagentCallOutput {
                result: ToolResult::completed(
                    &tool_request,
                    format!("Subagent status: success\n\n{output}"),
                    false,
                ),
                tool_request,
                description,
                task: Some(completed_task),
                status: RunStatus::Success,
                event_output: Some(output),
                event_error: None,
                cost_tracker,
                child_budget_usage: child.budget_usage,
            }
        }
        RunStatus::Cancelled => {
            let mut error = child
                .error
                .unwrap_or_else(|| "subagent ended with status Cancelled".to_string());
            append_worktree_outcome(&mut error, worktree.as_ref());
            RuntimeSubagentCallOutput {
                result: ToolResult::cancelled(
                    &tool_request,
                    format!("Subagent status: Cancelled\n\n{error}"),
                    None,
                ),
                tool_request,
                description,
                task: Some(completed_task),
                status: RunStatus::Cancelled,
                event_output: child.final_message,
                event_error: Some(error),
                cost_tracker,
                child_budget_usage: child.budget_usage,
            }
        }
        status => {
            let mut error = child
                .error
                .unwrap_or_else(|| format!("subagent ended with status {status:?}"));
            append_worktree_outcome(&mut error, worktree.as_ref());
            RuntimeSubagentCallOutput {
                result: ToolResult::failed_after_start(
                    &tool_request,
                    format!("Subagent status: {status:?}\n\n{error}"),
                    None,
                ),
                tool_request,
                description,
                task: Some(completed_task),
                status: RunStatus::Failed,
                event_output: child.final_message,
                event_error: Some(error),
                cost_tracker,
                child_budget_usage: child.budget_usage,
            }
        }
    }
}

fn cancelled_before_start(invocation: RuntimeSubagentInvocation) -> RuntimeSubagentCallOutput {
    let result = ToolResult::cancelled_before_start(
        &invocation.tool_request,
        "the subagent invocation was cancelled before dispatch",
    );
    RuntimeSubagentCallOutput {
        tool_request: invocation.tool_request,
        description: invocation.request.description,
        task: None,
        status: RunStatus::Cancelled,
        result,
        event_output: None,
        event_error: None,
        cost_tracker: CostTracker::new(invocation.config.model.as_deref()),
        child_budget_usage: None,
    }
}

fn failed_before_start(
    invocation: RuntimeSubagentInvocation,
    error: impl Into<String>,
) -> RuntimeSubagentCallOutput {
    let error = error.into();
    let result = ToolResult::failed_before_start(&invocation.tool_request, &error, None);
    RuntimeSubagentCallOutput {
        tool_request: invocation.tool_request,
        description: invocation.request.description,
        task: None,
        status: RunStatus::Failed,
        result,
        event_output: None,
        event_error: Some(error),
        cost_tracker: CostTracker::new(invocation.config.model.as_deref()),
        child_budget_usage: None,
    }
}

pub(crate) fn append_worktree_outcome(output: &mut String, outcome: Option<&WorktreeOutcome>) {
    if let Some(outcome) = outcome {
        let status = if outcome.preserved {
            "preserved"
        } else {
            "cleaned"
        };
        output.push_str(&format!(
            "\n\nWorktree {status}: {}",
            outcome.path.display()
        ));
    }
}

pub(crate) fn validate_subagent_output_schema(
    description: &str,
    schema: Option<&Value>,
    output: &str,
) -> Result<(), String> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let value = serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_string()));
    validate_json_schema_subset(schema, &value, "$").map_err(|error| {
        format!("subagent output schema validation failed for {description}: {error}")
    })
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_continuation::{AgentAttemptId, ToolBoundary};
    use crate::runtime_surface::RuntimeSubagentActivityIngress;

    #[derive(Debug, Default)]
    struct RecordingActivityIngress {
        events: Mutex<Vec<SubagentActivityEvent>>,
    }

    impl RuntimeSubagentActivityIngress for RecordingActivityIngress {
        fn owner(&self) -> SubagentActivityOwner {
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("task-sync-activity").expect("task id"),
                task_revision: TaskRevision::try_new(1).expect("revision"),
                authority_digest: crate::runtime_surface::Sha256Digest::new([9; 32]),
            }
        }

        fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()> {
            self.events
                .lock()
                .expect("recording ingress lock")
                .push(event);
            Ok(())
        }
    }

    #[test]
    fn runtime_activity_sink_acknowledges_the_original_source_event() {
        let ingress = Arc::new(RecordingActivityIngress::default());
        let sink = RuntimeSubagentActivitySink {
            ingress: ingress.clone(),
        };
        let event = SubagentActivityEvent::new(
            SurfaceTaskId::try_new("task-sync-activity").expect("task id"),
            SurfaceSubagentId::try_new("subagent-sync-activity").expect("subagent id"),
            AgentAttemptId::new(),
            1,
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("task-sync-activity").expect("task id"),
                task_revision: TaskRevision::try_new(1).expect("revision"),
                authority_digest: crate::runtime_surface::Sha256Digest::new([9; 32]),
            },
            SubagentActivityPayload::Started {
                description: DisplayText::new("inspect the runtime"),
            },
        );

        sink.publish(event.clone())
            .expect("ingress acknowledgement");

        assert_eq!(
            ingress.events.lock().expect("recorded events").as_slice(),
            &[event]
        );
    }

    #[test]
    fn inherited_sync_worktree_is_preserved_on_finish() {
        let execution = SyncWorktreeExecution::Inherited(WorktreeBinding {
            repo_root: "/missing/repo".to_string(),
            path: "/missing/inherited-worktree".to_string(),
        });

        let outcome = execution
            .finish()
            .expect("inherited worktree finish")
            .expect("worktree outcome");

        assert!(outcome.preserved);
        assert_eq!(outcome.path, PathBuf::from("/missing/inherited-worktree"));
    }

    #[test]
    fn continuation_write_retry_refreshes_revision_conflicts() {
        let registry = TaskRegistry::new("revision-retry".to_string());
        let task = registry.create_subagent("revision retry".to_string(), None);
        let coordinator =
            ChildAgentCoordinator::with_owner_id(registry, "revision-retry-owner".to_string())
                .expect("coordinator");
        let prepared = coordinator
            .create(CreateContinuationInput {
                continuation_id: Some(AgentContinuationId::new()),
                parent_task_id: None,
                task_id: task.id,
                prompt_id: AgentPromptId::new(),
                compatibility: ContinuationCompatibility {
                    subagent_type: "general".to_string(),
                    model: None,
                    isolation: SubagentIsolation::None,
                    effective_cwd: std::env::temp_dir().display().to_string(),
                    worktree: None,
                    compatibility_hash: crate::runtime_surface::Sha256Digest::new([3; 32]),
                },
            })
            .expect("prepared continuation");
        let lease = coordinator.acquire(&prepared).expect("continuation lease");
        let first_boundary = ToolBoundary::SafeToRetry {
            tool_call_id: Some("tool-1".to_string()),
        };
        let first = coordinator
            .commit_tool_boundary(&lease, lease.revision, first_boundary.clone())
            .expect("first boundary");
        let duplicate = coordinator
            .commit_tool_boundary(&lease, lease.revision, first_boundary)
            .expect("idempotent boundary retry");
        assert_eq!(duplicate.revision, first.revision);

        let second = commit_continuation_write_with_retry(
            &coordinator,
            &lease,
            lease.revision,
            |revision| {
                coordinator.commit_tool_boundary(
                    &lease,
                    revision,
                    ToolBoundary::SafeToRetry {
                        tool_call_id: Some("tool-2".to_string()),
                    },
                )
            },
        )
        .expect("revision conflict retry");

        assert!(second.revision > first.revision);
    }
}
